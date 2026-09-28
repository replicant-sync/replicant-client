//! One websocket connection, run in its own task and tagged with the core's socket generation.

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::{self, Message};

use crate::engine::types::ServerError;

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

pub struct SocketHandle {
    gen: u64,
    outgoing: mpsc::UnboundedSender<String>,
    task: JoinHandle<()>,
}

impl SocketHandle {
    pub fn open(url: String, gen: u64, events: mpsc::Sender<SocketEvent>) -> SocketHandle {
        let (outgoing, mut outgoing_rx) = mpsc::unbounded_channel::<String>();
        let task = tokio::spawn(async move {
            let ws = match tokio_tungstenite::connect_async(url.as_str()).await {
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
                    text = outgoing_rx.recv() => match text {
                        Some(text) => {
                            if sink.send(Message::Text(text)).await.is_err() {
                                break;
                            }
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
            task,
        }
    }

    pub fn gen(&self) -> u64 {
        self.gen
    }

    /// Queues a text frame; false once the socket task has ended.
    pub fn send(&self, text: String) -> bool {
        self.outgoing.send(text).is_ok()
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl Drop for SocketHandle {
    fn drop(&mut self) {
        self.task.abort();
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
        let socket = SocketHandle::open(server.url.clone(), 1, events_tx);
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
        let socket = SocketHandle::open(server.url.clone(), 3, events_tx);
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
        let _socket = SocketHandle::open(url, 2, events_tx);
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
        let _forbidden = SocketHandle::open(url, 1, events_tx.clone());
        assert_eq!(next(&mut events).await, SocketEvent::Closed { gen: 1 });

        let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_url = format!("ws://{}", unused.local_addr().unwrap());
        drop(unused);
        let _dial = SocketHandle::open(dead_url, 2, events_tx);
        assert_eq!(next(&mut events).await, SocketEvent::Closed { gen: 2 });
    }

    #[tokio::test]
    async fn dropping_the_handle_ends_a_stalled_dial_without_events() {
        let url = silent_server().await;
        let (events_tx, mut events) = mpsc::channel(16);
        let socket = SocketHandle::open(url, 1, events_tx);
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
        let socket = SocketHandle::open(server.url.clone(), 1, events_tx);
        assert_eq!(next(&mut events).await, SocketEvent::Opened { gen: 1 });
        let mut conn = server.accept().await;
        drop(socket);
        let after_drop = timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("aborted task drops its sender");
        assert_eq!(after_drop, None);
        conn.wait_closed().await;
    }
}
