//! Phoenix v2 JSON framing for one socket: `[join_ref, ref, topic, event, payload]`.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::wire::{
    ChangesPage, GetChangesSince, GetDocument, GetSnapshot, JoinAuth, JoinReply, ReplyEnvelope,
    SnapshotPage, UploadRequest, CHANNEL_TOPIC,
};
use crate::engine::machine::{Request, Response};
use crate::engine::types::{Change, DocEnvelope, Scope, ServerError};

const PHOENIX_TOPIC: &str = "phoenix";

#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Reply {
        req: u64,
        result: Result<Response, ServerError>,
    },
    /// The join succeeded; the driver checks `user_id` before telling the core.
    Joined {
        req: u64,
        user_id: Uuid,
    },
    Push(Change),
    /// A `change` push that did not decode; `scope` if its payload named one.
    UnreadablePush {
        scope: Option<Scope>,
    },
    /// The server closed or crashed the channel; the socket is no longer usable.
    ChannelClosed,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ReplyKind {
    Join,
    Changes,
    Snapshot,
    Upload,
    Document,
    Heartbeat,
}

#[derive(Debug, Default)]
pub struct Codec {
    join_ref: Option<String>,
    pending: HashMap<u64, ReplyKind>,
}

impl Codec {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn encode(
        &mut self,
        req: u64,
        request: &Request,
        auth: &JoinAuth,
        now_unix: i64,
    ) -> String {
        let reference = req.to_string();
        let (kind, topic, event, payload) = match request {
            Request::Join => {
                self.join_ref = Some(reference.clone());
                (
                    ReplyKind::Join,
                    CHANNEL_TOPIC,
                    "phx_join",
                    to_value(auth.sign(now_unix)),
                )
            }
            Request::Heartbeat => (ReplyKind::Heartbeat, PHOENIX_TOPIC, "heartbeat", json!({})),
            Request::GetChangesSince {
                scope,
                cursor,
                limit,
            } => (
                ReplyKind::Changes,
                CHANNEL_TOPIC,
                "get_changes_since",
                to_value(GetChangesSince {
                    scope,
                    cursor: *cursor,
                    limit: *limit,
                }),
            ),
            Request::GetSnapshot { scope, page_token } => (
                ReplyKind::Snapshot,
                CHANNEL_TOPIC,
                "get_snapshot",
                to_value(GetSnapshot {
                    scope,
                    page_token: page_token.as_deref(),
                }),
            ),
            Request::Upload(upload) => (
                ReplyKind::Upload,
                CHANNEL_TOPIC,
                "upload",
                to_value(UploadRequest {
                    upload_id: upload.upload_id,
                    doc_id: upload.doc_id,
                    kind: upload.kind,
                    base_hash: upload.base_hash.as_deref(),
                    payload: &upload.payload,
                }),
            ),
            Request::GetDocument { doc_id } => (
                ReplyKind::Document,
                CHANNEL_TOPIC,
                "get_document",
                to_value(GetDocument { doc_id: *doc_id }),
            ),
        };
        self.pending.insert(req, kind);
        let join_ref = if topic == CHANNEL_TOPIC {
            self.join_ref.clone()
        } else {
            None
        };
        json!([join_ref, reference, topic, event, payload]).to_string()
    }

    pub fn decode(&mut self, text: &str) -> Option<Incoming> {
        let (_join_ref, reference, topic, event, payload): (
            Option<String>,
            Option<String>,
            String,
            String,
            Value,
        ) = serde_json::from_str(text).ok()?;
        match event.as_str() {
            "phx_reply" => {
                let req: u64 = reference?.parse().ok()?;
                let kind = self.pending.remove(&req)?;
                Some(decode_reply(req, kind, payload))
            }
            "change" if topic == CHANNEL_TOPIC => {
                let scope = payload
                    .get("scope")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Some(match serde_json::from_value(payload) {
                    Ok(change) => Incoming::Push(change),
                    Err(_) => Incoming::UnreadablePush { scope },
                })
            }
            "phx_error" | "phx_close" if topic == CHANNEL_TOPIC => Some(Incoming::ChannelClosed),
            _ => None,
        }
    }
}

fn to_value(payload: impl Serialize) -> Value {
    serde_json::to_value(payload).expect("request payloads serialize")
}

fn decode_reply(req: u64, kind: ReplyKind, payload: Value) -> Incoming {
    let Ok(envelope) = serde_json::from_value::<ReplyEnvelope>(payload) else {
        return Incoming::Reply {
            req,
            result: Err(protocol_error()),
        };
    };
    if envelope.status != "ok" {
        let error = serde_json::from_value(envelope.response).unwrap_or_else(|_| protocol_error());
        return Incoming::Reply {
            req,
            result: Err(error),
        };
    }
    let response = envelope.response;
    let decoded = match kind {
        ReplyKind::Join => {
            return match serde_json::from_value::<JoinReply>(response) {
                Ok(join) => Incoming::Joined {
                    req,
                    user_id: join.user_id,
                },
                Err(_) => Incoming::Reply {
                    req,
                    result: Err(protocol_error()),
                },
            }
        }
        ReplyKind::Heartbeat => Ok(Response::HeartbeatOk),
        ReplyKind::Changes => {
            serde_json::from_value::<ChangesPage>(response).map(|page| Response::Changes {
                changes: page.changes,
                next_cursor: page.next_cursor,
                has_more: page.has_more,
            })
        }
        ReplyKind::Snapshot => {
            serde_json::from_value::<SnapshotPage>(response).map(|page| Response::SnapshotPage {
                docs: page.docs,
                snapshot_seq: page.snapshot_seq,
                next_page_token: page.next_page_token,
            })
        }
        ReplyKind::Upload => {
            serde_json::from_value::<DocEnvelope>(response).map(Response::Uploaded)
        }
        ReplyKind::Document => {
            serde_json::from_value::<DocEnvelope>(response).map(Response::Document)
        }
    };
    Incoming::Reply {
        req,
        result: decoded.map_err(|_| protocol_error()),
    }
}

/// Transient by construction: a frame the client cannot read is retried, never halts.
fn protocol_error() -> ServerError {
    ServerError::new("protocol_error")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn auth() -> JoinAuth {
        JoinAuth {
            email: "a@b.c".into(),
            api_key: "rpa_k".into(),
            api_secret: "rps_s".into(),
        }
    }

    fn changes_request() -> Request {
        Request::GetChangesSince {
            scope: "own".into(),
            cursor: 0,
            limit: 500,
        }
    }

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn channel_frames_carry_the_join_ref_and_heartbeats_use_the_phoenix_topic() {
        let mut codec = Codec::new();
        let join = parse(&codec.encode(4, &Request::Join, &auth(), 1));
        assert_eq!(join[0], json!("4"));
        assert_eq!(join[1], json!("4"));
        assert_eq!(join[2], json!("sync:v2"));
        assert_eq!(join[3], json!("phx_join"));

        let changes = parse(&codec.encode(5, &changes_request(), &auth(), 1));
        assert_eq!(changes[0], json!("4"));
        assert_eq!(changes[1], json!("5"));

        let heartbeat = parse(&codec.encode(6, &Request::Heartbeat, &auth(), 1));
        assert_eq!(heartbeat, json!([null, "6", "phoenix", "heartbeat", {}]));
    }

    #[test]
    fn heartbeat_reply_decodes_as_heartbeat_ok() {
        let mut codec = Codec::new();
        codec.encode(6, &Request::Heartbeat, &auth(), 1);
        let reply = json!([null, "6", "phoenix", "phx_reply", {"status": "ok", "response": {}}]);
        assert_eq!(
            codec.decode(&reply.to_string()),
            Some(Incoming::Reply {
                req: 6,
                result: Ok(Response::HeartbeatOk)
            })
        );
    }

    #[test]
    fn a_reply_is_matched_once_and_unknown_refs_are_ignored() {
        let mut codec = Codec::new();
        codec.encode(2, &changes_request(), &auth(), 1);
        let reply = json!(["1", "2", "sync:v2", "phx_reply",
            {"status": "ok", "response": {"changes": [], "next_cursor": 9, "has_more": false}}])
        .to_string();
        assert!(matches!(
            codec.decode(&reply),
            Some(Incoming::Reply {
                req: 2,
                result: Ok(Response::Changes { next_cursor: 9, .. })
            })
        ));
        assert_eq!(codec.decode(&reply), None);
        let unknown = json!(["1", "77", "sync:v2", "phx_reply", {"status": "ok", "response": {}}]);
        assert_eq!(codec.decode(&unknown.to_string()), None);
    }

    #[test]
    fn unreadable_replies_are_transient_protocol_errors() {
        let mut codec = Codec::new();
        codec.encode(2, &changes_request(), &auth(), 1);
        let bad_ok = json!(["1", "2", "sync:v2", "phx_reply",
            {"status": "ok", "response": {"bogus": 1}}]);
        let Some(Incoming::Reply {
            result: Err(error), ..
        }) = codec.decode(&bad_ok.to_string())
        else {
            panic!("expected an error reply");
        };
        assert_eq!(error.code, "protocol_error");
        assert!(!error.is_fatal);

        codec.encode(3, &changes_request(), &auth(), 1);
        let reasoned = json!(["1", "3", "sync:v2", "phx_reply",
            {"status": "error", "response": {"reason": "unmatched topic"}}]);
        let Some(Incoming::Reply {
            result: Err(error), ..
        }) = codec.decode(&reasoned.to_string())
        else {
            panic!("expected an error reply");
        };
        assert_eq!(error.code, "protocol_error");
    }

    #[test]
    fn channel_error_or_close_ends_the_channel() {
        let mut codec = Codec::new();
        for event in ["phx_error", "phx_close"] {
            let frame = json!(["1", "1", "sync:v2", event, {}]);
            assert_eq!(
                codec.decode(&frame.to_string()),
                Some(Incoming::ChannelClosed),
                "{event}"
            );
        }
    }

    #[test]
    fn unknown_events_and_malformed_text_are_ignored() {
        let mut codec = Codec::new();
        let presence = json!(["1", null, "sync:v2", "presence_diff", {}]);
        assert_eq!(codec.decode(&presence.to_string()), None);
        assert_eq!(codec.decode("not json"), None);
        assert_eq!(codec.decode(&json!({"topic": "sync:v2"}).to_string()), None);
    }

    #[test]
    fn unreadable_push_reports_its_scope() {
        let mut codec = Codec::new();
        let scoped = json!(["1", null, "sync:v2", "change", {"scope": "own", "seq": "x"}]);
        assert_eq!(
            codec.decode(&scoped.to_string()),
            Some(Incoming::UnreadablePush {
                scope: Some("own".into())
            })
        );
        let unscoped = json!(["1", null, "sync:v2", "change", {"seq": "x"}]);
        assert_eq!(
            codec.decode(&unscoped.to_string()),
            Some(Incoming::UnreadablePush { scope: None })
        );
    }

    #[test]
    fn join_ok_without_user_id_is_a_protocol_error() {
        let mut codec = Codec::new();
        codec.encode(1, &Request::Join, &auth(), 1);
        let reply = json!(["1", "1", "sync:v2", "phx_reply",
            {"status": "ok", "response": {"protocol_version": 2}}]);
        let Some(Incoming::Reply {
            req: 1,
            result: Err(error),
        }) = codec.decode(&reply.to_string())
        else {
            panic!("expected the join to fail");
        };
        assert_eq!(error.code, "protocol_error");
        assert!(!error.is_fatal);
    }

    #[test]
    fn non_envelope_reply_payload_is_a_protocol_error() {
        let mut codec = Codec::new();
        codec.encode(2, &changes_request(), &auth(), 1);
        let reply = json!(["1", "2", "sync:v2", "phx_reply", ["not", "an", "envelope"]]);
        let Some(Incoming::Reply {
            req: 2,
            result: Err(error),
        }) = codec.decode(&reply.to_string())
        else {
            panic!("expected an error reply");
        };
        assert_eq!(error.code, "protocol_error");
    }

    #[test]
    fn phx_error_on_the_phoenix_topic_is_ignored() {
        let mut codec = Codec::new();
        let frame = json!([null, null, "phoenix", "phx_error", {}]);
        assert_eq!(codec.decode(&frame.to_string()), None);
    }
}
