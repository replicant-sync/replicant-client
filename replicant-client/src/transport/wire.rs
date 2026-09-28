//! Protocol v2 payloads as fixed by replicant-server 0.5.0 (`lib/replicant_server/sync`).

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use uuid::Uuid;

use crate::engine::types::{Change, DocEnvelope, Seq, UploadKind};

pub const PROTOCOL_VERSION: &str = "2";
pub const CHANNEL_TOPIC: &str = "sync:v2";
/// Selects Phoenix's V2 JSON serializer (array frames).
const SERIALIZER_VSN: &str = "2.0.0";

/// Credentials for the `sync:v2` join. Each join is signed with a fresh timestamp.
/// Holds `api_secret`, so it derives neither `Debug` nor `PartialEq`.
pub struct JoinAuth {
    pub email: String,
    pub api_key: String,
    pub api_secret: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JoinPayload {
    pub email: String,
    pub api_key: String,
    pub signature: String,
    pub timestamp: i64,
}

impl JoinAuth {
    /// Hex HMAC-SHA256 over `"{timestamp}.{email}.{api_key}."` (the body is empty for a join).
    pub fn sign(&self, timestamp: i64) -> JoinPayload {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.api_secret.as_bytes())
            .expect("HMAC accepts any key size");
        mac.update(format!("{timestamp}.{}.{}.", self.email, self.api_key).as_bytes());
        JoinPayload {
            email: self.email.clone(),
            api_key: self.api_key.clone(),
            signature: hex::encode(mac.finalize().into_bytes()),
            timestamp,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct JoinReply {
    pub user_id: Uuid,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChangesPage {
    pub changes: Vec<Change>,
    pub next_cursor: Seq,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SnapshotPage {
    pub docs: Vec<DocEnvelope>,
    pub snapshot_seq: Seq,
    pub next_page_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GetChangesSince<'a> {
    pub scope: &'a str,
    pub cursor: Seq,
    pub limit: u32,
}

#[derive(Debug, Serialize)]
pub struct GetSnapshot<'a> {
    pub scope: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_token: Option<&'a str>,
}

#[derive(Debug, Serialize)]
pub struct UploadRequest<'a> {
    pub upload_id: Uuid,
    pub doc_id: Uuid,
    pub kind: UploadKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_hash: Option<&'a str>,
    pub payload: &'a Value,
}

#[derive(Debug, Serialize)]
pub struct GetDocument {
    pub doc_id: Uuid,
}

/// Payload of every `phx_reply` frame.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ReplyEnvelope {
    pub status: String,
    pub response: Value,
}

/// `server_url` may be http(s) or ws(s), with or without the `/socket/websocket` path.
pub fn socket_url(server_url: &str, client_id: Uuid) -> Result<String, String> {
    let ws_url = if let Some(rest) = server_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if let Some(rest) = server_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else {
        server_url.to_string()
    };
    let mut url =
        url::Url::parse(&ws_url).map_err(|e| format!("invalid server url {server_url}: {e}"))?;
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err(format!("unsupported server url scheme: {server_url}"));
    }
    if !url.path().ends_with("/socket/websocket") {
        let path = format!("{}/socket/websocket", url.path().trim_end_matches('/'));
        url.set_path(&path);
    }
    url.query_pairs_mut()
        .append_pair("protocol_version", PROTOCOL_VERSION)
        .append_pair("client_id", &client_id.to_string())
        .append_pair("vsn", SERIALIZER_VSN);
    Ok(url.into())
}

/// `replicant-client/<version> (<host app> <host version>)`, the host part omitted when unset.
/// Characters a header value cannot carry become `?`.
pub fn user_agent(host_app: &str, host_version: &str) -> String {
    let crate_part = format!("replicant-client/{}", env!("CARGO_PKG_VERSION"));
    let host = format!("{host_app} {host_version}");
    let host = host.trim();
    let agent = if host.is_empty() {
        crate_part
    } else {
        format!("{crate_part} ({host})")
    };
    agent
        .chars()
        .map(|c| {
            if c == ' ' || c.is_ascii_graphic() {
                c
            } else {
                '?'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::types::ServerError;
    use serde_json::json;

    #[test]
    fn join_signature_matches_the_server_hmac_scheme() {
        let auth = JoinAuth {
            email: "wire@example.com".into(),
            api_key: "rpa_key".into(),
            api_secret: "rps_test".into(),
        };
        let payload = auth.sign(1_767_225_600);
        let expected = "a3ffd0937b146736f3b60ffb28b943cf66c9d45d8c5e401fac9044d10db6f904";
        assert_eq!(
            serde_json::to_value(&payload).unwrap(),
            json!({
                "email": "wire@example.com",
                "api_key": "rpa_key",
                "signature": expected,
                "timestamp": 1_767_225_600
            })
        );
    }

    #[test]
    fn socket_url_adds_path_version_client_and_serializer() {
        let id = Uuid::from_u128(1);
        let query = format!("protocol_version=2&client_id={id}&vsn=2.0.0");
        assert_eq!(
            socket_url("http://localhost:4000", id).unwrap(),
            format!("ws://localhost:4000/socket/websocket?{query}")
        );
        assert_eq!(
            socket_url("https://sync.example.com/", id).unwrap(),
            format!("wss://sync.example.com/socket/websocket?{query}")
        );
        assert_eq!(
            socket_url("wss://sync.example.com/socket/websocket", id).unwrap(),
            format!("wss://sync.example.com/socket/websocket?{query}")
        );
        assert!(socket_url("ftp://sync.example.com", id).is_err());
        assert!(socket_url("not a url", id).is_err());
    }

    #[test]
    fn optional_request_fields_are_omitted_not_null() {
        assert_eq!(
            serde_json::to_value(GetSnapshot {
                scope: "own",
                page_token: None
            })
            .unwrap(),
            json!({"scope": "own"})
        );
        let payload = json!({"title": "t"});
        let create = UploadRequest {
            upload_id: Uuid::nil(),
            doc_id: Uuid::nil(),
            kind: UploadKind::Create,
            base_hash: None,
            payload: &payload,
        };
        assert_eq!(
            serde_json::to_value(create).unwrap(),
            json!({
                "upload_id": Uuid::nil(),
                "doc_id": Uuid::nil(),
                "kind": "create",
                "payload": {"title": "t"}
            })
        );
    }

    #[test]
    fn user_agent_names_the_crate_and_the_host() {
        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(
            user_agent("Entonal Studio", "2.0.1"),
            format!("replicant-client/{version} (Entonal Studio 2.0.1)")
        );
        assert_eq!(user_agent("", ""), format!("replicant-client/{version}"));
        assert_eq!(
            user_agent("Entonal\u{e9}", "1\n"),
            format!("replicant-client/{version} (Entonal? 1)")
        );
    }

    #[test]
    fn server_error_reads_known_fields_and_ignores_the_rest() {
        let error: ServerError = serde_json::from_value(json!({
            "code": "hash_mismatch",
            "is_fatal": false,
            "current_hash": "h",
            "current_seq": 3,
            "doc_id": Uuid::nil(),
            "scope": "own"
        }))
        .unwrap();
        assert_eq!(error.code, "hash_mismatch");
        assert_eq!(error.current_hash.as_deref(), Some("h"));
        assert_eq!(error.current_seq, Some(3));
        assert_eq!(error.retry_after_ms, None);

        let bare: ServerError = serde_json::from_value(json!({"code": "internal"})).unwrap();
        assert!(!bare.is_fatal);
    }
}
