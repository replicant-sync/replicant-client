//! One websocket connection, run in its own task and tagged with the core's socket generation.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::USER_AGENT;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};

use crate::engine::types::ServerError;

/// How long an explicitly closed socket may take to send its Close frame before it is aborted.
pub const CLOSE_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq)]
pub enum SocketEvent {
    Opened {
        gen: u64,
    },
    /// The upgrade was answered with an error the core must classify (HTTP 426).
    Refused {
        gen: u64,
        error: ServerError,
    },
    Text {
        gen: u64,
        text: String,
    },
    /// Dial failed, the peer closed, or a read or write failed. Sent at most once.
    Closed {
        gen: u64,
    },
}

enum Outgoing {
    Text(String),
    Close,
}

pub struct SocketHandle {
    gen: u64,
    outgoing: mpsc::UnboundedSender<Outgoing>,
    /// `None` once `close` has detached the task.
    task: Option<JoinHandle<()>>,
}

impl SocketHandle {
    pub fn open(
        url: String,
        user_agent: String,
        gen: u64,
        events: mpsc::Sender<SocketEvent>,
    ) -> SocketHandle {
        let (outgoing, mut outgoing_rx) = mpsc::unbounded_channel::<Outgoing>();
        let task = tokio::spawn(async move {
            let Ok(mut request) = url.into_client_request() else {
                let _ = events.send(SocketEvent::Closed { gen }).await;
                return;
            };
            if let Ok(value) = HeaderValue::from_str(&user_agent) {
                request.headers_mut().insert(USER_AGENT, value);
            }
            let dialed = tokio::select! {
                dialed = tokio_tungstenite::connect_async(request) => dialed,
                // Nothing but a close can be queued before the socket opens.
                _ = outgoing_rx.recv() => return,
            };
            let ws = match dialed {
                Ok((ws, _response)) => ws,
                Err(error) => {
                    let event = match refusal(&error) {
                        Some(error) => SocketEvent::Refused { gen, error },
                        None => SocketEvent::Closed { gen },
                    };
                    let _ = events.send(event).await;
                    return;
                }
            };
            if events.send(SocketEvent::Opened { gen }).await.is_err() {
                return;
            }
            let (mut sink, mut stream) = ws.split();
            loop {
                tokio::select! {
                    outgoing = outgoing_rx.recv() => match outgoing {
                        Some(Outgoing::Text(text)) => {
                            if sink.send(Message::Text(text)).await.is_err() {
                                break;
                            }
                        }
                        Some(Outgoing::Close) => {
                            let _ = sink.send(Message::Close(None)).await;
                            return;
                        }
                        None => break,
                    },
                    message = stream.next() => match message {
                        Some(Ok(Message::Text(text))) => {
                            if events.send(SocketEvent::Text { gen, text }).await.is_err() {
                                return;
                            }
                        }
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        Some(Ok(_)) => {}
                    },
                }
            }
            let _ = events.send(SocketEvent::Closed { gen }).await;
        });
        SocketHandle {
            gen,
            outgoing,
            task: Some(task),
        }
    }

    pub fn gen(&self) -> u64 {
        self.gen
    }

    /// Queues a text frame; false once the socket task has ended.
    pub fn send(&self, text: String) -> bool {
        self.outgoing.send(Outgoing::Text(text)).is_ok()
    }

    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Sends a Close frame and detaches the task: it ends on its own, reporting no event, or is
    /// aborted after `CLOSE_GRACE`. The returned handle only lets a caller await that end.
    pub fn close(mut self) -> JoinHandle<()> {
        let task = self
            .task
            .take()
            .expect("the task is only taken by close or drop");
        let _ = self.outgoing.send(Outgoing::Close);
        let abort = task.abort_handle();
        tokio::spawn(async move {
            tokio::time::sleep(CLOSE_GRACE).await;
            abort.abort();
        });
        task
    }
}

impl Drop for SocketHandle {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn refusal(error: &tungstenite::Error) -> Option<ServerError> {
    match error {
        tungstenite::Error::Http(response) if response.status().as_u16() == 426 => {
            let mut update_required = ServerError::new("update_required");
            update_required.is_fatal = true;
            Some(update_required)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::test_server::{refusing_server, silent_server, FakeServer};
    use serde_json::json;
    use std::time::Duration;
    use tokio::time::timeout;

    const UA: &str = "replicant-client/test";

    async fn next(events: &mut mpsc::Receiver<SocketEvent>) -> SocketEvent {
        timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("event within 5 s")
            .expect("socket task still holds the sender")
    }

    async fn until_finished(socket: &SocketHandle) {
        timeout(Duration::from_secs(5), async {
            while !socket.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("socket task ends within 5 s");
    }

    #[tokio::test]
    async fn opens_and_exchanges_text_frames() {
        let mut server = FakeServer::start().await;
        let (events_tx, mut events) = mpsc::channel(16);
        let socket = SocketHandle::open(server.url.clone(), UA.into(), 1, events_tx);
        assert_eq!(next(&mut events).await, SocketEvent::Opened { gen: 1 });
        let mut conn = server.accept().await;
        assert_eq!(conn.query, "");

        assert!(socket.send(json!(["1", "1", "sync:v2", "phx_join", {}]).to_string()));
        assert_eq!(conn.recv_json().await[3], json!("phx_join"));

        let push = json!([null, null, "sync:v2", "change", {}]);
        conn.send_json(push.clone()).await;
        assert_eq!(
            next(&mut events).await,
            SocketEvent::Text {
                gen: 1,
                text: push.to_string()
            }
        );
    }

    #[tokio::test]
    async fn peer_close_reports_closed_and_later_sends_fail() {
        let mut server = FakeServer::start().await;
        let (events_tx, mut events) = mpsc::channel(16);
        let socket = SocketHandle::open(server.url.clone(), UA.into(), 3, events_tx);
        assert_eq!(next(&mut events).await, SocketEvent::Opened { gen: 3 });
        server.accept().await.close().await;
        assert_eq!(next(&mut events).await, SocketEvent::Closed { gen: 3 });
        until_finished(&socket).await;
        assert!(!socket.send("late".into()));
    }

    #[tokio::test]
    async fn http_426_is_refused_with_update_required() {
        let url = refusing_server("426 Upgrade Required").await;
        let (events_tx, mut events) = mpsc::channel(16);
        let _socket = SocketHandle::open(url, UA.into(), 2, events_tx);
        let SocketEvent::Refused { gen: 2, error } = next(&mut events).await else {
            panic!("expected Refused for gen 2");
        };
        assert_eq!(error.code, "update_required");
        assert!(error.is_fatal);
    }

    #[tokio::test]
    async fn other_http_errors_and_dial_failures_are_plain_closes() {
        let url = refusing_server("403 Forbidden").await;
        let (events_tx, mut events) = mpsc::channel(16);
        let _forbidden = SocketHandle::open(url, UA.into(), 1, events_tx.clone());
        assert_eq!(next(&mut events).await, SocketEvent::Closed { gen: 1 });

        let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_url = format!("ws://{}", unused.local_addr().unwrap());
        drop(unused);
        let _dial = SocketHandle::open(dead_url, UA.into(), 2, events_tx);
        assert_eq!(next(&mut events).await, SocketEvent::Closed { gen: 2 });
    }

    #[tokio::test]
    async fn dropping_the_handle_ends_a_stalled_dial_without_events() {
        let url = silent_server().await;
        let (events_tx, mut events) = mpsc::channel(16);
        let socket = SocketHandle::open(url, UA.into(), 1, events_tx);
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(socket);
        let after_drop = timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("aborted task drops its sender");
        assert_eq!(after_drop, None);
    }

    #[tokio::test]
    async fn dropping_an_open_handle_ends_the_task_without_events() {
        let mut server = FakeServer::start().await;
        let (events_tx, mut events) = mpsc::channel(16);
        let socket = SocketHandle::open(server.url.clone(), UA.into(), 1, events_tx);
        assert_eq!(next(&mut events).await, SocketEvent::Opened { gen: 1 });
        let mut conn = server.accept().await;
        drop(socket);
        let after_drop = timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("aborted task drops its sender");
        assert_eq!(after_drop, None);
        conn.wait_closed().await;
    }

    #[tokio::test]
    async fn dial_sends_the_user_agent() {
        let mut server = FakeServer::start().await;
        let (events_tx, mut events) = mpsc::channel(16);
        let _socket = SocketHandle::open(
            server.url.clone(),
            "replicant-client/9.9 (Test 1.0)".into(),
            1,
            events_tx,
        );
        assert_eq!(next(&mut events).await, SocketEvent::Opened { gen: 1 });
        assert_eq!(
            server.accept().await.user_agent.as_deref(),
            Some("replicant-client/9.9 (Test 1.0)")
        );
    }

    #[tokio::test]
    async fn explicit_close_sends_a_close_frame() {
        let mut server = FakeServer::start().await;
        let (events_tx, mut events) = mpsc::channel(16);
        let socket = SocketHandle::open(server.url.clone(), UA.into(), 1, events_tx);
        assert_eq!(next(&mut events).await, SocketEvent::Opened { gen: 1 });
        let mut conn = server.accept().await;
        let task = socket.close();
        assert!(matches!(conn.next_message().await, Some(Message::Close(_))));
        let _ = timeout(Duration::from_secs(5), task)
            .await
            .expect("a closed socket's task ends");
        let after_close = timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the ended task drops its sender");
        assert_eq!(after_close, None, "an explicit close reports no event");
    }

    #[tokio::test]
    async fn close_ends_within_the_grace_period_when_the_peer_never_reads() {
        let mut server = FakeServer::start().await;
        let (events_tx, mut events) = mpsc::channel(16);
        let socket = SocketHandle::open(server.url.clone(), UA.into(), 1, events_tx);
        assert_eq!(next(&mut events).await, SocketEvent::Opened { gen: 1 });
        let _never_read = server.accept().await;
        let megabyte = "x".repeat(1 << 20);
        for _ in 0..64 {
            assert!(socket.send(megabyte.clone()));
        }
        // Let the task block in a text write against the full TCP buffers.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = std::time::Instant::now();
        let task = socket.close();
        let _ = timeout(Duration::from_secs(5), task)
            .await
            .expect("the grace watchdog ends the task");
        assert!(
            started.elapsed() < CLOSE_GRACE + Duration::from_millis(500),
            "close took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn close_during_a_stalled_dial_ends_the_task() {
        let url = silent_server().await;
        let (events_tx, mut events) = mpsc::channel(16);
        let socket = SocketHandle::open(url, UA.into(), 1, events_tx);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let started = std::time::Instant::now();
        let _ = timeout(Duration::from_secs(5), socket.close())
            .await
            .expect("a stalled dial ends on close");
        assert!(started.elapsed() < CLOSE_GRACE);
        let after_close = timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the ended task drops its sender");
        assert_eq!(after_close, None);
    }
}
