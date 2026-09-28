//! Local websocket servers for transport tests.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

const WAIT: Duration = Duration::from_secs(5);

pub struct FakeServer {
    pub url: String,
    connections: mpsc::UnboundedReceiver<FakeConnection>,
}

pub struct FakeConnection {
    pub query: String,
    pub user_agent: Option<String>,
    ws: WebSocketStream<TcpStream>,
}

impl FakeServer {
    pub async fn start() -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (connection_tx, connections) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let mut query = String::new();
                let mut user_agent = None;
                let accepted = tokio_tungstenite::accept_hdr_async(
                    tcp,
                    |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
                        query = request.uri().query().unwrap_or_default().to_string();
                        user_agent = request
                            .headers()
                            .get("user-agent")
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string);
                        Ok(response)
                    },
                )
                .await;
                if let Ok(ws) = accepted {
                    let _ = connection_tx.send(FakeConnection {
                        query,
                        user_agent,
                        ws,
                    });
                }
            }
        });
        FakeServer { url, connections }
    }

    pub async fn accept(&mut self) -> FakeConnection {
        timeout(WAIT, self.connections.recv())
            .await
            .expect("client connects within 5 s")
            .expect("server task alive")
    }
}

impl FakeConnection {
    pub async fn recv_json(&mut self) -> Value {
        loop {
            match timeout(WAIT, self.ws.next())
                .await
                .expect("frame within 5 s")
            {
                Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).unwrap(),
                Some(Ok(_)) => continue,
                other => panic!("connection ended: {other:?}"),
            }
        }
    }

    pub async fn send_json(&mut self, frame: Value) {
        self.ws
            .send(Message::Text(frame.to_string()))
            .await
            .unwrap();
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }

    /// The next websocket message of any kind; `None` once the client has gone.
    pub async fn next_message(&mut self) -> Option<Message> {
        timeout(WAIT, self.ws.next())
            .await
            .expect("message within 5 s")
            .and_then(Result::ok)
    }

    /// Resolves once the client side has gone away.
    pub async fn wait_closed(&mut self) {
        timeout(WAIT, async {
            while let Some(Ok(message)) = self.ws.next().await {
                if message.is_close() {
                    break;
                }
            }
        })
        .await
        .expect("client closes within 5 s");
    }
}

/// Answers every upgrade with `status_line` (e.g. "426 Upgrade Required") and a JSON body.
pub async fn refusing_server(status_line: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            let mut request = [0u8; 4096];
            let _ = tcp.read(&mut request).await;
            let body = r#"{"code":"update_required","is_fatal":true}"#;
            let response = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = tcp.write_all(response.as_bytes()).await;
        }
    });
    url
}

/// Accepts TCP connections and never answers the websocket handshake.
pub async fn silent_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((tcp, _)) = listener.accept().await {
            held.push(tcp);
        }
    });
    url
}
