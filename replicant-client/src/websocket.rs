use crate::events::EventDispatcher;
use hmac::{Hmac, Mac};
use phoenix_channels_client::{Channel, Event, Payload, Socket, Topic};
use replicant_core::{
    errors::ClientError,
    models::{Document, DocumentPatch},
    protocol::{ChangeEvent, ChangeEventType, ClientMessage, ErrorCode, ServerMessage},
    SyncResult,
};
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use url::Url;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const JOIN_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct WebSocketClient {
    channel: Arc<Channel>,
    tx: mpsc::Sender<ServerMessage>,
}

pub struct WebSocketReceiver {
    rx: mpsc::Receiver<ServerMessage>,
}

impl WebSocketClient {
    pub async fn connect(
        server_url: &str,
        email: &str,
        client_id: Uuid,
        api_key: &str,
        api_secret: &str,
        event_dispatcher: Option<Arc<EventDispatcher>>,
        is_connected: Arc<AtomicBool>,
    ) -> SyncResult<(Self, WebSocketReceiver)> {
        Self::connect_with_hmac(
            server_url,
            email,
            client_id,
            api_key,
            api_secret,
            event_dispatcher,
            is_connected,
        )
        .await
    }

    pub async fn connect_with_hmac(
        server_url: &str,
        email: &str,
        client_id: Uuid,
        api_key: &str,
        api_secret: &str,
        event_dispatcher: Option<Arc<EventDispatcher>>,
        is_connected: Arc<AtomicBool>,
    ) -> SyncResult<(Self, WebSocketReceiver)> {
        let ws_url = Self::to_websocket_url(server_url)?;

        if let Some(ref d) = event_dispatcher {
            d.emit_connection_attempted(&ws_url);
        }

        // Connect socket
        let url = Url::parse(&ws_url).map_err(|e| ws_err(format!("Invalid URL: {}", e)))?;
        let socket = Socket::spawn(url, None, None)
            .await
            .map_err(|e| ws_err(format!("Socket spawn failed: {:?}", e)))?;

        socket.connect(CONNECT_TIMEOUT).await.map_err(|e| {
            if let Some(ref d) = event_dispatcher {
                d.emit_sync_error(&format!("Connection failed: {:?}", e));
            }
            ws_err(format!("Connect failed: {:?}", e))
        })?;

        // Join channel with HMAC auth
        let timestamp = chrono::Utc::now().timestamp();
        let signature = Self::create_hmac_signature(api_secret, timestamp, email, api_key, "");
        let join_payload = json!({
            "email": email,
            "api_key": api_key,
            "signature": signature,
            "timestamp": timestamp
        });

        let channel = socket
            .channel(
                Topic::from_string("sync:main".to_string()),
                Some(to_payload(&join_payload)?),
            )
            .await
            .map_err(|e| ws_err(format!("Channel create failed: {:?}", e)))?;

        channel.join(JOIN_TIMEOUT).await.map_err(|e| {
            if let Some(ref d) = event_dispatcher {
                d.emit_sync_error(&format!("Join failed: {:?}", e));
            }
            ws_err(format!("Join failed: {:?}", e))
        })?;

        is_connected.store(true, Ordering::Relaxed);
        if let Some(ref d) = event_dispatcher {
            d.emit_connection_succeeded(&ws_url);
        }

        let (tx, rx) = mpsc::channel::<ServerMessage>(100);
        Self::setup_broadcast_handlers(&channel, tx.clone(), is_connected);

        // Emit auth success
        let _ = tx
            .send(ServerMessage::AuthSuccess {
                session_id: Uuid::new_v4(),
                client_id,
            })
            .await;

        Ok((Self { channel, tx }, WebSocketReceiver { rx }))
    }

    fn to_websocket_url(server_url: &str) -> SyncResult<String> {
        let url = match server_url {
            s if s.starts_with("http://") => s.replace("http://", "ws://"),
            s if s.starts_with("https://") => s.replace("https://", "wss://"),
            s if s.starts_with("ws://") || s.starts_with("wss://") => s.to_string(),
            _ => return Err(ws_err(format!("Invalid URL scheme: {}", server_url))),
        };

        Ok(if url.contains("/socket/websocket") {
            url
        } else {
            format!("{}/socket/websocket", url.trim_end_matches('/'))
        })
    }

    fn setup_broadcast_handlers(
        channel: &Arc<Channel>,
        tx: mpsc::Sender<ServerMessage>,
        is_connected: Arc<AtomicBool>,
    ) {
        let events = channel.events();
        let tx_clone = tx;
        let is_connected_clone = is_connected;

        tokio::spawn(async move {
            loop {
                match events.event().await {
                    Ok(event_payload) => {
                        let event_name = event_payload.event.to_string();
                        let payload_json = payload_to_value(&event_payload.payload);

                        match event_name.as_str() {
                            "document_created" => {
                                if let Some(doc) = payload_json.as_ref().and_then(json_to_document)
                                {
                                    let _ = tx_clone
                                        .send(ServerMessage::DocumentCreated { document: doc })
                                        .await;
                                }
                            }
                            "document_updated" => {
                                if let Some(patch) = payload_json.as_ref().and_then(json_to_patch) {
                                    let _ = tx_clone
                                        .send(ServerMessage::DocumentUpdated { patch })
                                        .await;
                                }
                            }
                            "document_deleted" => {
                                if let Some(id) = payload_json
                                    .as_ref()
                                    .and_then(|j| j.get("document_id")?.as_str())
                                    .and_then(|s| Uuid::parse_str(s).ok())
                                {
                                    let _ = tx_clone
                                        .send(ServerMessage::DocumentDeleted { document_id: id })
                                        .await;
                                }
                            }
                            "phx_close" => {
                                is_connected_clone.store(false, Ordering::Relaxed);
                                let _ = tx_clone
                                    .send(ServerMessage::Error {
                                        code: ErrorCode::ServerError,
                                        message: "Channel closed".to_string(),
                                    })
                                    .await;
                            }
                            _ => {}
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    pub async fn send(&self, message: ClientMessage) -> SyncResult<()> {
        match message {
            ClientMessage::Authenticate { .. } => Ok(()), // Handled in join
            ClientMessage::CreateDocument { document } => self.create_document(document).await,
            ClientMessage::UpdateDocument { patch } => self.update_document(patch).await,
            ClientMessage::DeleteDocument { document_id } => {
                self.delete_document(document_id).await
            }
            ClientMessage::RequestFullSync => self.request_full_sync().await,
            ClientMessage::RequestSync { .. } => self.request_full_sync().await,
            ClientMessage::GetChangesSince {
                last_sequence,
                limit,
            } => self.get_changes_since(last_sequence, limit).await,
            ClientMessage::AckChanges { .. } => Ok(()),
            ClientMessage::Ping => {
                let _ = self.tx.send(ServerMessage::Pong).await;
                Ok(())
            }
        }
    }

    async fn create_document(&self, document: Document) -> SyncResult<()> {
        let payload = json!({"id": document.id.to_string(), "content": document.content});
        let resp = self.call("create_document", &payload).await;

        let (success, error) = match &resp {
            Ok(j) => (j.get("document_id").is_some(), None),
            Err(e) => (false, Some(format!("{:?}", e))),
        };

        let _ = self
            .tx
            .send(ServerMessage::DocumentCreatedResponse {
                document_id: document.id,
                success,
                error,
            })
            .await;
        Ok(())
    }

    async fn update_document(&self, patch: DocumentPatch) -> SyncResult<()> {
        let payload = json!({
            "document_id": patch.document_id.to_string(),
            "patch": patch.patch,
            "content_hash": patch.content_hash
        });
        let resp = self.call("update_document", &payload).await;

        let (success, sync_revision, error) = match &resp {
            Ok(j) => {
                let rev = j.get("sync_revision").and_then(|v| v.as_i64());
                (rev.is_some(), rev, None)
            }
            Err(e) => (false, None, Some(format!("{:?}", e))),
        };

        let _ = self
            .tx
            .send(ServerMessage::DocumentUpdatedResponse {
                document_id: patch.document_id,
                success,
                error,
                sync_revision,
            })
            .await;
        Ok(())
    }

    async fn delete_document(&self, document_id: Uuid) -> SyncResult<()> {
        let payload = json!({"document_id": document_id.to_string()});
        let resp = self.call("delete_document", &payload).await;

        let (success, error) = match &resp {
            Ok(_) => (true, None),
            Err(e) => (false, Some(format!("{:?}", e))),
        };

        let _ = self
            .tx
            .send(ServerMessage::DocumentDeletedResponse {
                document_id,
                success,
                error,
            })
            .await;
        Ok(())
    }

    async fn request_full_sync(&self) -> SyncResult<()> {
        let resp = self.call("request_full_sync", &json!({})).await;

        match resp {
            Ok(j) => {
                if let Some(docs) = j.get("documents").and_then(|v| v.as_array()) {
                    for doc_json in docs {
                        if let Some(document) = json_to_document(doc_json) {
                            let _ = self.tx.send(ServerMessage::SyncDocument { document }).await;
                        }
                    }
                }
                let synced_count = j
                    .get("documents")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                let _ = self
                    .tx
                    .send(ServerMessage::SyncComplete { synced_count })
                    .await;
            }
            Err(e) => {
                let _ = self
                    .tx
                    .send(ServerMessage::Error {
                        code: ErrorCode::ServerError,
                        message: format!("Full sync failed: {:?}", e),
                    })
                    .await;
            }
        }
        Ok(())
    }

    async fn get_changes_since(&self, last_sequence: u64, limit: Option<u32>) -> SyncResult<()> {
        let mut payload = json!({"last_sequence": last_sequence});
        if let Some(l) = limit {
            payload["limit"] = json!(l);
        }

        let resp = self.call("get_changes_since", &payload).await;

        match resp {
            Ok(j) => {
                let events = j
                    .get("events")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(json_to_change_event).collect())
                    .unwrap_or_default();
                let latest_sequence = j
                    .get("latest_sequence")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                let _ = self
                    .tx
                    .send(ServerMessage::Changes {
                        events,
                        latest_sequence,
                        has_more: false,
                    })
                    .await;
            }
            Err(e) => {
                let _ = self
                    .tx
                    .send(ServerMessage::Error {
                        code: ErrorCode::ServerError,
                        message: format!("Get changes failed: {:?}", e),
                    })
                    .await;
            }
        }
        Ok(())
    }

    async fn call(&self, event: &str, payload: &Value) -> Result<Value, String> {
        self.channel
            .call(
                Event::from_string(event.to_string()),
                to_payload(payload).map_err(|e| format!("{:?}", e))?,
                CALL_TIMEOUT,
            )
            .await
            .map_err(|e| format!("{:?}", e))
            .and_then(|p| payload_to_value(&p).ok_or_else(|| "Invalid response".to_string()))
    }

    fn create_hmac_signature(
        secret: &str,
        timestamp: i64,
        email: &str,
        api_key: &str,
        body: &str,
    ) -> String {
        let mut mac =
            HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
        mac.update(format!("{}.{}.{}.{}", timestamp, email, api_key, body).as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

impl WebSocketReceiver {
    pub async fn receive(&mut self) -> SyncResult<Option<ServerMessage>> {
        Ok(self.rx.recv().await)
    }

    pub async fn forward_to(mut self, tx: mpsc::Sender<ServerMessage>) -> SyncResult<()> {
        tracing::info!("CLIENT: WebSocket receiver forwarder started");
        while let Some(msg) = self.receive().await? {
            tracing::info!(
                "CLIENT: Received message: {:?}",
                std::mem::discriminant(&msg)
            );
            if tx.send(msg).await.is_err() {
                tracing::error!("CLIENT: Failed to forward message");
                break;
            }
        }
        tracing::warn!("CLIENT: WebSocket receiver forwarder terminated");
        Ok(())
    }
}

// Helper functions
fn ws_err(msg: String) -> replicant_core::errors::SyncError {
    ClientError::WebSocket(msg).into()
}

fn to_payload(v: &Value) -> SyncResult<Payload> {
    Payload::json_from_serialized(v.to_string())
        .map_err(|e| ws_err(format!("Payload error: {:?}", e)))
}

fn payload_to_value(p: &Payload) -> Option<Value> {
    match p {
        Payload::JSONPayload { json } => Some(Value::from(json.clone())),
        Payload::Binary { .. } => None,
    }
}

fn json_to_document(j: &Value) -> Option<Document> {
    Some(Document {
        id: Uuid::parse_str(j.get("id")?.as_str()?).ok()?,
        user_id: j
            .get("user_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap_or_else(Uuid::nil),
        content: j.get("content")?.clone(),
        sync_revision: j.get("sync_revision")?.as_i64()?,
        content_hash: j
            .get("content_hash")
            .and_then(|v| v.as_str())
            .map(String::from),
        title: j.get("title").and_then(|v| v.as_str()).map(String::from),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        deleted_at: None,
    })
}

fn json_to_patch(j: &Value) -> Option<DocumentPatch> {
    let patch_value = j.get("patch")?;
    let patch: json_patch::Patch = serde_json::from_value(patch_value.clone()).ok()?;
    Some(DocumentPatch {
        document_id: Uuid::parse_str(j.get("document_id")?.as_str()?).ok()?,
        patch,
        content_hash: j
            .get("content_hash")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_default(),
    })
}

fn json_to_change_event(j: &Value) -> Option<ChangeEvent> {
    Some(ChangeEvent {
        sequence: j.get("sequence")?.as_u64()?,
        document_id: Uuid::parse_str(j.get("document_id")?.as_str()?).ok()?,
        user_id: Uuid::nil(),
        event_type: match j.get("event_type")?.as_str()? {
            "create" => ChangeEventType::Create,
            "update" => ChangeEventType::Update,
            "delete" => ChangeEventType::Delete,
            _ => return None,
        },
        forward_patch: j.get("forward_patch").cloned(),
        reverse_patch: j.get("reverse_patch").cloned(),
        created_at: j
            .get("server_timestamp")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now),
    })
}
