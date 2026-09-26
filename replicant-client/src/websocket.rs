use crate::error_code::{error_code_for_reason, ReplicantErrorCode};
use crate::events::EventDispatcher;
use hmac::{Hmac, Mac};
use phoenix_channels_client::{
    CallError, Channel, ChannelJoinError, ChannelStatus, Event, Payload, Socket, StatusesError,
    Topic,
};
use replicant_core::{
    errors::ClientError,
    models::{Document, DocumentPatch, ServerDocumentPatch},
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
    _public_channel: Arc<Channel>,
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
        user_id: Uuid,
        api_key: &str,
        api_secret: &str,
        event_dispatcher: Option<Arc<EventDispatcher>>,
        is_connected: Arc<AtomicBool>,
    ) -> SyncResult<(Self, WebSocketReceiver)> {
        Self::connect_with_hmac(
            server_url,
            email,
            client_id,
            user_id,
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
        user_id: Uuid,
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
                d.emit_sync_error(
                    ReplicantErrorCode::ConnectionFailed,
                    &format!("Connection failed: {:?}", e),
                );
            }
            ws_err(format!("Connect failed: {:?}", e))
        })?;

        // Join channel with HMAC auth; the topic and payload both carry the
        // caller's (already-adopted) user id.
        let timestamp = chrono::Utc::now().timestamp();
        let signature = Self::create_hmac_signature(api_secret, timestamp, email, api_key, "");
        let join_payload = Self::build_join_payload(email, api_key, &signature, timestamp, user_id);

        // Join per-user channel
        let channel = socket
            .channel(
                Topic::from_string(format!("sync:user:{}", user_id)),
                Some(to_payload(&join_payload)?),
            )
            .await
            .map_err(|e| ws_err(format!("Channel create failed: {:?}", e)))?;

        let join_reply = channel.join(JOIN_TIMEOUT).await.map_err(|e| {
            if let Some(ref d) = event_dispatcher {
                d.emit_sync_error(
                    error_code_for_join_reject(&e),
                    &format!("Join failed: {:?}", e),
                );
            }
            ws_err(format!("Join failed: {:?}", e))
        })?;

        // Identity drift check: every join reply names the credential's user
        // id. A mismatch means the local identity diverged from the account —
        // refuse to sync rather than silently re-stamp anything.
        let reply_value = payload_to_value(&join_reply).unwrap_or_else(|| json!({}));
        if let Err(msg) = Self::verify_reply_identity(user_id, &reply_value) {
            if let Some(ref d) = event_dispatcher {
                d.emit_sync_error(ReplicantErrorCode::IdentityDrift, &msg);
            }
            let _ = channel.leave().await;
            return Err(ws_err(msg));
        }

        // Join public channel for public document events
        let public_channel = socket
            .channel(
                Topic::from_string("sync:public".to_string()),
                Some(to_payload(&join_payload)?),
            )
            .await
            .map_err(|e| ws_err(format!("Public channel create failed: {:?}", e)))?;

        public_channel.join(JOIN_TIMEOUT).await.map_err(|e| {
            if let Some(ref d) = event_dispatcher {
                d.emit_sync_error(
                    error_code_for_join_reject(&e),
                    &format!("Public channel join failed: {:?}", e),
                );
            }
            ws_err(format!("Public channel join failed: {:?}", e))
        })?;

        is_connected.store(true, Ordering::Relaxed);

        let (tx, rx) = mpsc::channel::<ServerMessage>(100);
        Self::setup_broadcast_handlers(&channel, tx.clone(), is_connected.clone());
        Self::setup_broadcast_handlers(&public_channel, tx.clone(), is_connected.clone());

        // The phoenix client freezes the join payload (and its timestamp) at
        // channel creation and replays it on every auto-rejoin; once the stamp
        // is older than the server's clock-skew window every rejoin is rejected
        // forever. A channel-level rejoin never flips is_connected, so watch the
        // channel status and hand off to our own reconnect loop (which mints a
        // fresh timestamp) when the library gets stuck rejoining.
        Self::setup_status_watcher(&channel, is_connected.clone());
        Self::setup_status_watcher(&public_channel, is_connected);

        // Emit auth success
        let _ = tx
            .send(ServerMessage::AuthSuccess {
                session_id: Uuid::new_v4(),
                client_id,
            })
            .await;

        Ok((
            Self {
                channel,
                _public_channel: public_channel,
                tx,
            },
            WebSocketReceiver { rx },
        ))
    }

    /// Build the channel join payload; always carries the adopted user id.
    pub(crate) fn build_join_payload(
        email: &str,
        api_key: &str,
        signature: &str,
        timestamp: i64,
        user_id: Uuid,
    ) -> Value {
        json!({
            "email": email,
            "api_key": api_key,
            "signature": signature,
            "timestamp": timestamp,
            "user_id": user_id.to_string(),
        })
    }

    /// `Ok` when the join reply's `user_id` (if present) matches ours; `Err`
    /// with a description when it differs or cannot be parsed. A reply naming
    /// a different user means local identity diverged from the account — the
    /// caller must refuse to sync, never silently re-stamp.
    pub(crate) fn verify_reply_identity(local: Uuid, reply: &Value) -> Result<(), String> {
        match reply.get("user_id").and_then(|v| v.as_str()) {
            None => Ok(()),
            Some(s) => match Uuid::parse_str(s) {
                Ok(server_id) if server_id == local => Ok(()),
                Ok(server_id) => Err(format!(
                    "identity drift: server reports user {} but local identity is {}; \
                     refusing to sync",
                    server_id, local
                )),
                Err(_) => Err(format!(
                    "identity drift: server sent unparseable user_id {:?}; refusing to sync",
                    s
                )),
            },
        }
    }

    /// Whether a channel status means the phoenix client is stuck rejoining
    /// with its frozen (stale-timestamp) payload and our reconnect loop should
    /// take over.
    fn should_reconnect_on_status(status: &ChannelStatus) -> bool {
        matches!(status, ChannelStatus::WaitingToRejoin { .. })
    }

    fn setup_status_watcher(channel: &Arc<Channel>, is_connected: Arc<AtomicBool>) {
        let statuses = channel.statuses();

        tokio::spawn(async move {
            loop {
                match statuses.status().await {
                    Ok(status) => {
                        if Self::should_reconnect_on_status(&status) {
                            is_connected.store(false, Ordering::Relaxed);
                        }
                    }
                    // Only a closed status channel ends the watch; a lagged
                    // receiver just skips ahead to the next status.
                    Err(StatusesError::NoMoreStatuses) => break,
                    Err(_) => continue,
                }
            }
        });
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
                                match payload_json.as_ref().and_then(json_to_patch) {
                                    Some(patch) => {
                                        let _ = tx_clone
                                            .send(ServerMessage::DocumentUpdated { patch })
                                            .await;
                                    }
                                    None => {
                                        tracing::warn!(
                                            "dropping malformed document_updated payload: {:?}",
                                            payload_json
                                        );
                                    }
                                }
                            }
                            "document_deleted" => {
                                if let Some(id) = payload_json
                                    .as_ref()
                                    .and_then(|j| j.get("id")?.as_str())
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
            ClientMessage::GetDocument { id } => self.get_document(id).await,
            ClientMessage::Ping => {
                let _ = self.tx.send(ServerMessage::Pong).await;
                Ok(())
            }
        }
    }

    async fn create_document(&self, document: Document) -> SyncResult<()> {
        let payload = json!({"id": document.id.to_string(), "content": document.content});
        let resp = self.call("create_document", &payload).await;

        let (success, error, author_name, visibility, provenance) = match &resp {
            Ok(j) => (
                j.get("id").is_some(),
                None,
                j.get("author_name")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                j.get("visibility")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                j.get("provenance").filter(|v| !v.is_null()).cloned(),
            ),
            Err(e) => (false, Some(format!("{:?}", e)), None, None, None),
        };

        let _ = self
            .tx
            .send(ServerMessage::DocumentCreatedResponse {
                document_id: document.id,
                success,
                error,
                author_name,
                visibility,
                provenance,
            })
            .await;
        Ok(())
    }

    async fn update_document(&self, patch: DocumentPatch) -> SyncResult<()> {
        let payload = json!({
            "id": patch.document_id.to_string(),
            "patch": patch.patch,
            "content_hash": patch.content_hash
        });

        // Bypass the `call` helper (which stringifies CallError) so a
        // `hash_mismatch` rejection keeps the server state it carries. Without
        // it the client cannot rebase and the local edit is silently lost.
        let encoded = match to_payload(&payload) {
            Ok(p) => p,
            Err(e) => {
                let _ = self
                    .tx
                    .send(update_rejected(
                        patch.document_id,
                        format!("Update failed to encode request: {:?}", e),
                        Value::Null,
                    ))
                    .await;
                return Ok(());
            }
        };

        let result = self
            .channel
            .call(
                Event::from_string("update_document".to_string()),
                encoded,
                CALL_TIMEOUT,
            )
            .await;

        let message = match result {
            Ok(reply) => {
                let revision = payload_to_value(&reply)
                    .and_then(|j| j.get("sync_revision").and_then(|v| v.as_i64()));
                match revision {
                    Some(revision) => ServerMessage::DocumentUpdatedResponse {
                        document_id: patch.document_id,
                        success: true,
                        error: None,
                        sync_revision: Some(revision),
                        reason: None,
                        current_revision: None,
                        current_content: None,
                        current_hash: None,
                    },
                    None => update_rejected(
                        patch.document_id,
                        "Malformed update_document response".to_string(),
                        Value::Null,
                    ),
                }
            }
            Err(CallError::Reply { reply }) => {
                let body = payload_to_value(&reply).unwrap_or(Value::Null);
                let reason = body
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                update_rejected(
                    patch.document_id,
                    format!("Update rejected: {}", reason),
                    body,
                )
            }
            Err(e) => update_rejected(
                patch.document_id,
                format!("Update failed: {:?}", e),
                Value::Null,
            ),
        };

        let _ = self.tx.send(message).await;
        Ok(())
    }

    async fn delete_document(&self, document_id: Uuid) -> SyncResult<()> {
        let payload = json!({"id": document_id.to_string()});
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

    async fn get_document(&self, id: Uuid) -> SyncResult<()> {
        let msg = self.fetch_document(id).await;
        let _ = self.tx.send(msg).await;
        Ok(())
    }

    /// Fetch a full document and return the resulting message to the caller
    /// instead of routing it through the inbound channel. Resync needs the
    /// reply inline: routing it back through `tx` would lose the association
    /// with the document being resynced (`ServerMessage::Error` carries no id).
    pub async fn fetch_document(&self, id: Uuid) -> ServerMessage {
        // Bypass the `call` helper (which stringifies CallError) so a
        // `{"reason": "not_found"}` reply can be distinguished from a
        // transient transport/timeout failure.
        let payload = json!({"id": id.to_string()});
        let encoded = match to_payload(&payload) {
            Ok(p) => p,
            Err(e) => {
                return ServerMessage::Error {
                    code: ErrorCode::ServerError,
                    message: format!("Get document failed to encode request: {:?}", e),
                }
            }
        };

        let result = self
            .channel
            .call(
                Event::from_string("get_document".to_string()),
                encoded,
                CALL_TIMEOUT,
            )
            .await;

        match result {
            Ok(reply) => match payload_to_value(&reply) {
                Some(j) => match json_to_get_document_response(&j, id) {
                    Ok(msg) => msg,
                    Err(parse_err) => ServerMessage::Error {
                        code: ErrorCode::ServerError,
                        message: format!("Malformed get_document response: {}", parse_err),
                    },
                },
                None => ServerMessage::Error {
                    code: ErrorCode::ServerError,
                    message: "Invalid get_document response payload".to_string(),
                },
            },
            Err(CallError::Reply { reply: payload }) => {
                let reason = payload_to_value(&payload)
                    .and_then(|v| v.get("reason").and_then(|r| r.as_str().map(String::from)));
                match reason.as_deref() {
                    Some("not_found") => ServerMessage::Error {
                        code: ErrorCode::DocumentNotFound,
                        message: "Document not found".to_string(),
                    },
                    _ => ServerMessage::Error {
                        code: ErrorCode::ServerError,
                        message: format!("Get document failed: server error reply {:?}", reason),
                    },
                }
            }
            Err(e) => ServerMessage::Error {
                code: ErrorCode::ServerError,
                message: format!("Get document failed: {:?}", e),
            },
        }
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

/// A failed update ack, carrying whatever the server's error reply held.
///
/// `body` is the raw rejection payload; for `hash_mismatch` it also carries the
/// server's current revision, content and hash, which the client rebases onto.
/// Every field is optional so other reasons — and future ones — still parse.
fn update_rejected(document_id: Uuid, error: String, body: Value) -> ServerMessage {
    ServerMessage::DocumentUpdatedResponse {
        document_id,
        success: false,
        error: Some(error),
        sync_revision: None,
        reason: body
            .get("reason")
            .and_then(|v| v.as_str())
            .map(String::from),
        current_revision: body.get("current_revision").and_then(|v| v.as_i64()),
        current_content: body
            .get("current_content")
            .filter(|v| !v.is_null())
            .cloned(),
        current_hash: body
            .get("current_hash")
            .and_then(|v| v.as_str())
            .map(String::from),
    }
}

fn payload_to_value(p: &Payload) -> Option<Value> {
    match p {
        Payload::JSONPayload { json } => Some(Value::from(json.clone())),
        Payload::Binary { .. } => None,
    }
}

/// Derive a structured [`ReplicantErrorCode`] from a phoenix channel-join error.
///
/// A server rejection carries a JSON payload `{"reason": "<atom>"}` (see
/// `replicant_server` `Sync.Channel`); the reason is mapped through
/// [`error_code_for_reason`]. A rejection with no `reason` field is `Unknown`.
/// Join timeouts map to [`ReplicantErrorCode::Timeout`] and every other
/// transport/socket failure to [`ReplicantErrorCode::ConnectionFailed`].
pub fn error_code_for_join_reject(err: &ChannelJoinError) -> ReplicantErrorCode {
    match err {
        ChannelJoinError::Rejected { rejection } => payload_to_value(rejection)
            .as_ref()
            .and_then(|v| v.get("reason"))
            .and_then(|r| r.as_str())
            .map(error_code_for_reason)
            .unwrap_or(ReplicantErrorCode::Unknown),
        ChannelJoinError::Timeout => ReplicantErrorCode::Timeout,
        _ => ReplicantErrorCode::ConnectionFailed,
    }
}

fn json_to_document(j: &Value) -> Option<Document> {
    // The server always carries ownership in the sync envelope: a string
    // user_id (owned doc) or null (public doc). A payload missing the key
    // entirely is malformed — reject it rather than guess an owner.
    let Some(uid_value) = j.get("user_id") else {
        tracing::warn!(
            "dropping document payload without ownership envelope (id: {:?})",
            j.get("id")
        );
        return None;
    };
    let user_id = match uid_value {
        Value::Null => None,
        uid_value => Some(Uuid::parse_str(uid_value.as_str()?).ok()?),
    };
    Some(Document {
        id: Uuid::parse_str(j.get("id")?.as_str()?).ok()?,
        user_id,
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
        author_name: j
            .get("author_name")
            .and_then(|v| v.as_str())
            .map(String::from),
        visibility: j
            .get("visibility")
            .and_then(|v| v.as_str())
            .map(String::from),
        provenance: j.get("provenance").filter(|v| !v.is_null()).cloned(),
    })
}

fn json_to_patch(j: &Value) -> Option<ServerDocumentPatch> {
    let patch_value = j.get("patch")?;
    let patch: json_patch::Patch = serde_json::from_value(patch_value.clone()).ok()?;
    let id_str = j
        .get("document_id")
        .or_else(|| j.get("id"))
        .and_then(|v| v.as_str())?;
    Some(ServerDocumentPatch {
        document_id: Uuid::parse_str(id_str).ok()?,
        patch,
        sync_revision: j.get("sync_revision")?.as_i64()?,
        content_hash: j.get("content_hash")?.as_str().map(String::from)?,
    })
}

/// Parse a `get_document` success reply into `ServerMessage::GetDocumentResponse`.
///
/// `sync_revision` and `content_hash` are new fields with no legacy-compat
/// requirement: a reply missing either is malformed and must be rejected
/// rather than silently defaulted, since a defaulted revision would corrupt
/// downstream continuity checks. `id` falls back to the requested id (the
/// server always echoes it, but the caller already knows it either way).
fn json_to_get_document_response(j: &Value, requested_id: Uuid) -> Result<ServerMessage, String> {
    let id = j
        .get("id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or(requested_id);
    let content = j.get("content").cloned().unwrap_or(Value::Null);
    let sync_revision = j
        .get("sync_revision")
        .and_then(|v| v.as_i64())
        .ok_or("missing or invalid sync_revision")?;
    let content_hash = j
        .get("content_hash")
        .and_then(|v| v.as_str())
        .ok_or("missing or invalid content_hash")?
        .to_string();
    let deleted = j.get("deleted").and_then(|v| v.as_bool()).unwrap_or(false);

    Ok(ServerMessage::GetDocumentResponse {
        id,
        content,
        sync_revision,
        content_hash,
        deleted,
    })
}

fn json_to_change_event(j: &Value) -> Option<ChangeEvent> {
    Some(ChangeEvent {
        sequence: j.get("sequence")?.as_u64()?,
        document_id: Uuid::parse_str(j.get("id")?.as_str()?).ok()?,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_to_patch_reads_sync_revision_and_content_hash() {
        let j = serde_json::json!({
            "id": "71b2b712-7878-56ee-8323-43809b8198a5",
            "patch": [{"op": "replace", "path": "/title", "value": "T"}],
            "sync_revision": 7,
            "content_hash": "abc123"
        });
        let patch = json_to_patch(&j).unwrap();
        assert_eq!(
            patch.document_id.to_string(),
            "71b2b712-7878-56ee-8323-43809b8198a5"
        );
        assert_eq!(patch.sync_revision, 7);
        assert_eq!(patch.content_hash, "abc123");
    }

    #[test]
    fn json_to_patch_missing_sync_revision_is_none() {
        let j = serde_json::json!({
            "id": "71b2b712-7878-56ee-8323-43809b8198a5",
            "patch": [{"op": "replace", "path": "/title", "value": "T"}],
            "content_hash": "abc123"
        });
        assert!(json_to_patch(&j).is_none());
    }

    #[test]
    fn json_to_patch_missing_content_hash_is_none() {
        let j = serde_json::json!({
            "id": "71b2b712-7878-56ee-8323-43809b8198a5",
            "patch": [{"op": "replace", "path": "/title", "value": "T"}],
            "sync_revision": 7
        });
        assert!(json_to_patch(&j).is_none());
    }

    #[test]
    fn json_to_get_document_response_tolerates_unknown_fields() {
        let requested_id = Uuid::new_v4();
        let j = serde_json::json!({
            "id": requested_id.to_string(),
            "content": {"title": "T"},
            "sync_revision": 5,
            "content_hash": "abc123",
            "deleted": false,
            "future_field": {"nested": "data"}
        });
        let msg = json_to_get_document_response(&j, requested_id).unwrap();
        match msg {
            ServerMessage::GetDocumentResponse {
                id,
                sync_revision,
                content_hash,
                deleted,
                ..
            } => {
                assert_eq!(id, requested_id);
                assert_eq!(sync_revision, 5);
                assert_eq!(content_hash, "abc123");
                assert!(!deleted);
            }
            other => panic!("expected GetDocumentResponse, got {:?}", other),
        }
    }

    #[test]
    fn json_to_get_document_response_missing_sync_revision_is_error() {
        let requested_id = Uuid::new_v4();
        let j = serde_json::json!({
            "id": requested_id.to_string(),
            "content": {},
            "content_hash": "abc123",
            "deleted": false
        });
        assert!(json_to_get_document_response(&j, requested_id).is_err());
    }

    #[test]
    fn json_to_get_document_response_missing_content_hash_is_error() {
        let requested_id = Uuid::new_v4();
        let j = serde_json::json!({
            "id": requested_id.to_string(),
            "content": {},
            "sync_revision": 5,
            "deleted": false
        });
        assert!(json_to_get_document_response(&j, requested_id).is_err());
    }

    #[test]
    fn json_to_document_reads_attribution() {
        let j = serde_json::json!({
            "id": "71b2b712-7878-56ee-8323-43809b8198a5",
            "user_id": "71b2b712-7878-56ee-8323-43809b8198a5",
            "content": {"title": "T"},
            "sync_revision": 3,
            "content_hash": "abc",
            "author_name": "Sevish",
            "visibility": "public",
            "provenance": {"copied_from": "x"}
        });
        let doc = json_to_document(&j).unwrap();
        assert_eq!(doc.author_name.as_deref(), Some("Sevish"));
        assert_eq!(doc.visibility.as_deref(), Some("public"));
        assert!(doc.provenance.is_some());
    }

    #[test]
    fn json_to_document_tolerates_missing_attribution() {
        let j = serde_json::json!({
            "id": "71b2b712-7878-56ee-8323-43809b8198a5",
            "user_id": null,
            "content": {"title": "T"},
            "sync_revision": 1
        });
        let doc = json_to_document(&j).unwrap();
        assert_eq!(doc.user_id, None, "null user_id means a public document");
        assert_eq!(doc.author_name, None);
        assert_eq!(doc.visibility, None);
    }

    #[test]
    fn json_to_document_rejects_payload_without_user_id_key() {
        let j = serde_json::json!({
            "id": "71b2b712-7878-56ee-8323-43809b8198a5",
            "content": {"title": "T"},
            "sync_revision": 1
        });
        assert!(
            json_to_document(&j).is_none(),
            "ownership must come from the envelope, never be guessed"
        );
    }

    #[test]
    fn join_payload_always_includes_user_id() {
        let uid = Uuid::new_v4();
        let payload = WebSocketClient::build_join_payload("a@b.com", "key", "sig", 123, uid);
        assert_eq!(payload["user_id"], uid.to_string());
        assert_eq!(payload["email"], "a@b.com");
    }

    #[test]
    fn reply_identity_matching_id_is_ok() {
        let local = Uuid::new_v4();
        let reply = serde_json::json!({ "user_id": local.to_string() });
        assert!(WebSocketClient::verify_reply_identity(local, &reply).is_ok());
    }

    #[test]
    fn reply_identity_mismatch_is_drift_error() {
        let local = Uuid::new_v4();
        let other = Uuid::new_v4();
        let reply = serde_json::json!({ "user_id": other.to_string() });
        assert!(WebSocketClient::verify_reply_identity(local, &reply).is_err());
    }

    #[test]
    fn reply_identity_absent_is_tolerated() {
        let local = Uuid::new_v4();
        let reply = serde_json::json!({ "email": "a@b.com" });
        assert!(WebSocketClient::verify_reply_identity(local, &reply).is_ok());
    }

    #[test]
    fn reply_identity_garbage_is_drift_error() {
        let local = Uuid::new_v4();
        let reply = serde_json::json!({ "user_id": "not-a-uuid" });
        assert!(WebSocketClient::verify_reply_identity(local, &reply).is_err());
    }

    #[test]
    fn should_reconnect_on_waiting_to_rejoin() {
        let status = ChannelStatus::WaitingToRejoin {
            until: std::time::SystemTime::now(),
        };
        assert!(WebSocketClient::should_reconnect_on_status(&status));
    }

    #[test]
    fn should_not_reconnect_on_steady_statuses() {
        for status in [
            ChannelStatus::Joined,
            ChannelStatus::Joining,
            ChannelStatus::WaitingToJoin,
            ChannelStatus::WaitingForSocketToConnect,
            ChannelStatus::Leaving,
            ChannelStatus::Left,
        ] {
            assert!(!WebSocketClient::should_reconnect_on_status(&status));
        }
    }
}
