//! The driver's side of the socket: runs socket effects and turns socket events into core inputs.

use tokio::sync::mpsc;
use uuid::Uuid;

use super::codec::{Codec, Incoming};
use super::socket::{SocketEvent, SocketHandle};
use super::wire::JoinAuth;
use crate::engine::machine::{Input, Request};

const EVENT_BUFFER: usize = 64;

#[derive(Debug, PartialEq)]
#[allow(clippy::large_enum_variant)] // Input is the natural payload; boxing it would fight every match site.
pub enum Received {
    Input(Input),
    /// The join succeeded; the driver checks `user_id` before feeding `Reply { Ok(Joined) }`.
    Joined {
        req: u64,
        user_id: Uuid,
    },
}

pub struct Connection {
    url: String,
    auth: JoinAuth,
    current: Option<(SocketHandle, Codec)>,
    events_tx: mpsc::Sender<SocketEvent>,
    events_rx: mpsc::Receiver<SocketEvent>,
}

impl Connection {
    pub fn new(url: String, auth: JoinAuth) -> Connection {
        let (events_tx, events_rx) = mpsc::channel(EVENT_BUFFER);
        Connection {
            url,
            auth,
            current: None,
            events_tx,
            events_rx,
        }
    }

    /// `Effect::OpenSocket`. Replaces (and so aborts) any current socket.
    pub fn open(&mut self, gen: u64) {
        let socket = SocketHandle::open(self.url.clone(), gen, self.events_tx.clone());
        self.current = Some((socket, Codec::new()));
    }

    /// `Effect::CloseSocket`. Later events from that socket are dropped by `receive`.
    pub fn close(&mut self, gen: u64) {
        if self.current_gen() == Some(gen) {
            self.current = None;
        }
    }

    /// `Effect::Send`. Returns the input to feed back when the frame could not be queued.
    pub fn send(&mut self, req: u64, request: &Request, now_unix: i64) -> Option<Input> {
        let (socket, codec) = self.current.as_mut()?;
        let text = codec.encode(req, request, &self.auth, now_unix);
        if socket.send(text) {
            return None;
        }
        let gen = socket.gen();
        self.current = None;
        Some(Input::SocketClosed { gen })
    }

    /// Cancel-safe; use as a `select!` branch.
    pub async fn next_event(&mut self) -> SocketEvent {
        self.events_rx
            .recv()
            .await
            .expect("Connection keeps a sender alive")
    }

    pub fn receive(&mut self, event: SocketEvent) -> Option<Received> {
        let current_gen = self.current_gen()?;
        match event {
            SocketEvent::Opened { gen } if gen == current_gen => {
                Some(Received::Input(Input::SocketOpened { gen }))
            }
            SocketEvent::Refused { gen, error } if gen == current_gen => {
                self.current = None;
                Some(Received::Input(Input::ConnectRefused { gen, error }))
            }
            SocketEvent::Closed { gen } if gen == current_gen => {
                self.current = None;
                Some(Received::Input(Input::SocketClosed { gen }))
            }
            SocketEvent::Text { gen, text } if gen == current_gen => {
                let (_, codec) = self.current.as_mut()?;
                match codec.decode(&text)? {
                    Incoming::Reply { req, result } => {
                        Some(Received::Input(Input::Reply { req, result }))
                    }
                    Incoming::Joined { req, user_id } => Some(Received::Joined { req, user_id }),
                    Incoming::Push(change) => Some(Received::Input(Input::Push(change))),
                    Incoming::UnreadablePush { scope } => {
                        tracing::warn!(?scope, "unreadable change push");
                        Some(Received::Input(Input::UnreadablePush { scope }))
                    }
                    Incoming::ChannelClosed => {
                        self.current = None;
                        Some(Received::Input(Input::SocketClosed { gen }))
                    }
                }
            }
            _ => None,
        }
    }

    fn current_gen(&self) -> Option<u64> {
        self.current.as_ref().map(|(socket, _)| socket.gen())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::machine::Response;
    use crate::engine::types::{Upload, UploadKind};
    use crate::transport::test_server::{refusing_server, FakeConnection, FakeServer};
    use crate::transport::wire::socket_url;
    use serde_json::json;
    use std::time::Duration;
    use tokio::time::timeout;

    fn auth() -> JoinAuth {
        JoinAuth {
            email: "a@b.c".into(),
            api_key: "rpa_k".into(),
            api_secret: "rps_s".into(),
        }
    }

    /// Next event that survives generation filtering.
    async fn received(connection: &mut Connection) -> Received {
        timeout(Duration::from_secs(5), async {
            loop {
                let event = connection.next_event().await;
                if let Some(received) = connection.receive(event) {
                    return received;
                }
            }
        })
        .await
        .expect("input within 5 s")
    }

    async fn opened(server: &mut FakeServer) -> (Connection, FakeConnection) {
        let mut connection = Connection::new(server.url.clone(), auth());
        connection.open(1);
        assert_eq!(
            received(&mut connection).await,
            Received::Input(Input::SocketOpened { gen: 1 })
        );
        let conn = server.accept().await;
        (connection, conn)
    }

    #[tokio::test]
    async fn joins_requests_pushes_and_heartbeats_over_one_socket() {
        let mut server = FakeServer::start().await;
        let client_id = Uuid::from_u128(9);
        let mut connection = Connection::new(socket_url(&server.url, client_id).unwrap(), auth());
        connection.open(1);
        assert_eq!(
            received(&mut connection).await,
            Received::Input(Input::SocketOpened { gen: 1 })
        );
        let mut conn = server.accept().await;
        assert!(conn.query.contains("protocol_version=2"));
        assert!(conn.query.contains(&format!("client_id={client_id}")));
        assert!(conn.query.contains("vsn=2.0.0"));

        assert_eq!(connection.send(1, &Request::Join, 1_767_225_600), None);
        let join = conn.recv_json().await;
        assert_eq!(join[3], json!("phx_join"));
        assert_eq!(join[4]["email"], json!("a@b.c"));
        conn.send_json(json!(["1", "1", "sync:v2", "phx_reply",
            {"status": "ok", "response": {"user_id": Uuid::nil(), "protocol_version": 2}}]))
            .await;
        assert_eq!(
            received(&mut connection).await,
            Received::Joined {
                req: 1,
                user_id: Uuid::nil()
            }
        );

        let changes = Request::GetChangesSince {
            scope: "own".into(),
            cursor: 0,
            limit: 500,
        };
        assert_eq!(connection.send(2, &changes, 0), None);
        assert_eq!(conn.recv_json().await[3], json!("get_changes_since"));
        conn.send_json(json!(["1", "2", "sync:v2", "phx_reply",
            {"status": "ok", "response": {"changes": [], "next_cursor": 4, "has_more": false}}]))
            .await;
        assert_eq!(
            received(&mut connection).await,
            Received::Input(Input::Reply {
                req: 2,
                result: Ok(Response::Changes {
                    changes: vec![],
                    next_cursor: 4,
                    has_more: false
                })
            })
        );

        assert_eq!(connection.send(3, &Request::Heartbeat, 0), None);
        assert_eq!(
            conn.recv_json().await,
            json!([null, "3", "phoenix", "heartbeat", {}])
        );
        conn.send_json(
            json!([null, "3", "phoenix", "phx_reply", {"status": "ok", "response": {}}]),
        )
        .await;
        assert_eq!(
            received(&mut connection).await,
            Received::Input(Input::Reply {
                req: 3,
                result: Ok(Response::HeartbeatOk)
            })
        );

        conn.send_json(json!(["1", null, "sync:v2", "change", {
            "scope": "own", "seq": 5, "prev_seq": 4, "doc_id": Uuid::nil(), "kind": "delete",
            "doc": null, "client_id": null, "upload_id": null
        }]))
        .await;
        let Received::Input(Input::Push(change)) = received(&mut connection).await else {
            panic!("expected a push");
        };
        assert_eq!(change.seq, 5);
    }

    #[tokio::test]
    async fn events_from_other_generations_are_dropped() {
        let mut connection = Connection::new("ws://127.0.0.1:9".into(), auth());
        connection.open(1);
        connection.open(2);
        assert_eq!(connection.receive(SocketEvent::Opened { gen: 1 }), None);
        assert_eq!(
            connection.receive(SocketEvent::Text {
                gen: 1,
                text: "[]".into()
            }),
            None
        );
        assert_eq!(connection.receive(SocketEvent::Closed { gen: 1 }), None);
        connection.close(1);
        assert_eq!(
            connection.receive(SocketEvent::Opened { gen: 2 }),
            Some(Received::Input(Input::SocketOpened { gen: 2 }))
        );
        connection.close(2);
        assert_eq!(connection.receive(SocketEvent::Closed { gen: 2 }), None);
    }

    #[tokio::test]
    async fn http_426_becomes_connect_refused() {
        let url = refusing_server("426 Upgrade Required").await;
        let mut connection = Connection::new(url, auth());
        connection.open(4);
        let Received::Input(Input::ConnectRefused { gen: 4, error }) =
            received(&mut connection).await
        else {
            panic!("expected ConnectRefused for gen 4");
        };
        assert_eq!(error.code, "update_required");
        assert!(error.is_fatal);
    }

    #[tokio::test]
    async fn channel_error_closes_the_socket_and_later_frames_are_dropped() {
        let mut server = FakeServer::start().await;
        let (mut connection, mut conn) = opened(&mut server).await;
        conn.send_json(json!(["1", "1", "sync:v2", "phx_error", {}]))
            .await;
        assert_eq!(
            received(&mut connection).await,
            Received::Input(Input::SocketClosed { gen: 1 })
        );
        let late = json!(["1", null, "sync:v2", "phx_close", {}]).to_string();
        assert_eq!(
            connection.receive(SocketEvent::Text { gen: 1, text: late }),
            None
        );
        assert_eq!(connection.send(5, &Request::Heartbeat, 0), None);
    }

    #[tokio::test]
    async fn unreadable_push_becomes_an_input_with_its_scope() {
        let mut server = FakeServer::start().await;
        let (mut connection, mut conn) = opened(&mut server).await;
        conn.send_json(json!(["1", null, "sync:v2", "change", {"scope": "own", "seq": "x"}]))
            .await;
        assert_eq!(
            received(&mut connection).await,
            Received::Input(Input::UnreadablePush {
                scope: Some("own".into())
            })
        );
    }

    #[tokio::test]
    async fn server_drop_mid_request_yields_only_socket_closed() {
        let mut server = FakeServer::start().await;
        let (mut connection, mut conn) = opened(&mut server).await;
        let upload = Request::Upload(Upload {
            upload_id: Uuid::from_u128(1),
            doc_id: Uuid::from_u128(2),
            kind: UploadKind::Create,
            base_hash: None,
            payload: json!({"title": "t"}),
        });
        assert_eq!(connection.send(2, &upload, 0), None);
        assert_eq!(conn.recv_json().await[3], json!("upload"));
        conn.close().await;
        assert_eq!(
            received(&mut connection).await,
            Received::Input(Input::SocketClosed { gen: 1 })
        );
        let nothing_more = timeout(Duration::from_millis(300), connection.next_event()).await;
        assert!(
            nothing_more.is_err(),
            "no reply may be invented for the upload"
        );
    }

    #[tokio::test]
    async fn send_after_the_socket_task_ended_reports_socket_closed() {
        let mut server = FakeServer::start().await;
        let (mut connection, conn) = opened(&mut server).await;
        conn.close().await;
        // Take Closed off the channel without `receive`, as when the core sends first.
        let event = timeout(Duration::from_secs(5), connection.next_event())
            .await
            .unwrap();
        assert_eq!(event, SocketEvent::Closed { gen: 1 });
        let reported = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(input) = connection.send(9, &Request::Heartbeat, 0) {
                    return input;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("send fails once the task has ended");
        assert_eq!(reported, Input::SocketClosed { gen: 1 });
        assert_eq!(connection.receive(event), None, "reported once only");
    }

    #[tokio::test]
    async fn dropping_the_connection_closes_the_server_side() {
        let mut server = FakeServer::start().await;
        let (connection, mut conn) = opened(&mut server).await;
        drop(connection);
        conn.wait_closed().await;
    }
}
