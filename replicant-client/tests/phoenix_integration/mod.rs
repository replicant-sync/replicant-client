//! Integration tests for Rust client ↔ Phoenix server communication.
//!
//! These tests require a running Phoenix server and API credentials:
//!
//! ```bash
//! # Terminal 1: Start Phoenix server
//! cd replicant_server
//! mix deps.get
//! mix ecto.setup
//! mix phx.server
//!
//! # Generate credentials (one-time)
//! mix replicant.gen.credentials --name "integration-test"
//!
//! # Terminal 2: Run integration tests
//! cd replicant-client/replicant-client
//! REPLICANT_API_KEY="rpa_..." \
//! REPLICANT_API_SECRET="rps_..." \
//! RUN_INTEGRATION_TESTS=1 \
//! cargo test --test integration
//! ```

mod basic_sync_test;
mod conflict_test;
mod live_sync_test;
mod multi_client_test;

pub use serial_test::serial;

use hmac::{Hmac, Mac};
use phoenix_channels_client::{Channel, Event, Payload, Socket, Topic};
use replicant_core::models::Document;
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use url::Url;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// Default test credentials - set via environment variables
/// Generate with: mix replicant.gen.credentials --name "integration-test"
pub const TEST_EMAIL: &str = "integration-test@example.com";

pub fn test_api_key() -> String {
    std::env::var("REPLICANT_API_KEY")
        .expect("REPLICANT_API_KEY env var required for integration tests")
}

pub fn test_api_secret() -> String {
    std::env::var("REPLICANT_API_SECRET")
        .expect("REPLICANT_API_SECRET env var required for integration tests")
}

pub fn server_url() -> String {
    std::env::var("SYNC_SERVER_URL")
        .unwrap_or_else(|_| "ws://localhost:4000/socket/websocket".to_string())
}

pub fn skip_if_no_server() -> bool {
    std::env::var("RUN_INTEGRATION_TESTS").is_err()
}

/// Generate deterministic user ID from email (matches server's Auth.deterministic_user_id/1)
pub fn deterministic_user_id(email: &str) -> Uuid {
    const APP_ID: &str = "com.example.sync-task-list";
    let app_namespace = Uuid::new_v5(&Uuid::NAMESPACE_DNS, APP_ID.as_bytes());
    Uuid::new_v5(&app_namespace, email.as_bytes())
}

/// A broadcast event received from the server
#[derive(Debug)]
pub struct BroadcastEvent {
    pub event: String,
    pub payload: Value,
}

/// Test client for integration tests
pub struct TestClient {
    pub channel: Arc<Channel>,
    pub public_channel: Arc<Channel>,
    pub email: String,
    pub user_id: Uuid,
    broadcast_rx: mpsc::Receiver<BroadcastEvent>,
}

impl TestClient {
    pub async fn connect(email: &str) -> Result<Self, String> {
        Self::connect_with_credentials(email, &test_api_key(), &test_api_secret()).await
    }

    pub async fn connect_with_credentials(
        email: &str,
        api_key: &str,
        api_secret: &str,
    ) -> Result<Self, String> {
        let url = Url::parse(&server_url()).map_err(|e| format!("Invalid URL: {}", e))?;
        let user_id = deterministic_user_id(email);

        let socket = Socket::spawn(url, None, None)
            .await
            .map_err(|e| format!("Socket spawn failed: {:?}", e))?;

        socket
            .connect(Duration::from_secs(10))
            .await
            .map_err(|e| format!("Connect failed: {:?}", e))?;

        let timestamp = chrono::Utc::now().timestamp();
        let signature = create_hmac_signature(api_secret, timestamp, email, api_key);

        let join_payload = json!({
            "email": email,
            "api_key": api_key,
            "signature": signature,
            "timestamp": timestamp
        });

        // Join per-user channel
        let channel = socket
            .channel(
                Topic::from_string(format!("sync:user:{}", user_id)),
                Some(to_payload(&join_payload)?),
            )
            .await
            .map_err(|e| format!("User channel create failed: {:?}", e))?;

        channel
            .join(Duration::from_secs(10))
            .await
            .map_err(|e| format!("User channel join failed: {:?}", e))?;

        // Join public channel
        let public_channel = socket
            .channel(
                Topic::from_string("sync:public".to_string()),
                Some(to_payload(&join_payload)?),
            )
            .await
            .map_err(|e| format!("Public channel create failed: {:?}", e))?;

        public_channel
            .join(Duration::from_secs(10))
            .await
            .map_err(|e| format!("Public channel join failed: {:?}", e))?;

        // Set up broadcast event receiver
        let (tx, rx) = mpsc::channel::<BroadcastEvent>(100);
        Self::spawn_event_listener(&channel, tx.clone());
        Self::spawn_event_listener(&public_channel, tx);

        Ok(Self {
            channel,
            public_channel,
            email: email.to_string(),
            user_id,
            broadcast_rx: rx,
        })
    }

    fn spawn_event_listener(channel: &Arc<Channel>, tx: mpsc::Sender<BroadcastEvent>) {
        let events = channel.events();
        tokio::spawn(async move {
            loop {
                match events.event().await {
                    Ok(event_payload) => {
                        let event_name = event_payload.event.to_string();
                        let payload_json =
                            payload_to_value(&event_payload.payload).unwrap_or(Value::Null);
                        let _ = tx
                            .send(BroadcastEvent {
                                event: event_name,
                                payload: payload_json,
                            })
                            .await;
                    }
                    Err(_) => break,
                }
            }
        });
    }

    /// Receive the next broadcast event, with a timeout.
    /// Returns None if the timeout expires before an event arrives.
    pub async fn recv_broadcast(&mut self, timeout: Duration) -> Option<BroadcastEvent> {
        tokio::time::timeout(timeout, self.broadcast_rx.recv())
            .await
            .ok()
            .flatten()
    }

    /// Drain any pending broadcast events (call before the action you want to observe).
    pub async fn drain_broadcasts(&mut self) {
        while self
            .recv_broadcast(Duration::from_millis(50))
            .await
            .is_some()
        {}
    }

    pub async fn create_document(&self, content: Value) -> Result<Value, String> {
        let doc_id = Uuid::new_v4();
        let payload = json!({"id": doc_id.to_string(), "content": content});
        self.call("create_document", &payload).await
    }

    pub async fn create_document_with_id(&self, id: Uuid, content: Value) -> Result<Value, String> {
        let payload = json!({"id": id.to_string(), "content": content});
        self.call("create_document", &payload).await
    }

    pub async fn update_document(
        &self,
        document_id: Uuid,
        patch: Value,
        content_hash: &str,
    ) -> Result<Value, String> {
        let payload = json!({
            "id": document_id.to_string(),
            "patch": patch,
            "content_hash": content_hash
        });
        self.call("update_document", &payload).await
    }

    pub async fn delete_document(&self, document_id: Uuid) -> Result<Value, String> {
        let payload = json!({"id": document_id.to_string()});
        self.call("delete_document", &payload).await
    }

    pub async fn request_full_sync(&self) -> Result<Value, String> {
        self.call("request_full_sync", &json!({})).await
    }

    pub async fn get_changes_since(&self, last_sequence: u64) -> Result<Value, String> {
        let payload = json!({"last_sequence": last_sequence});
        self.call("get_changes_since", &payload).await
    }

    async fn call(&self, event: &str, payload: &Value) -> Result<Value, String> {
        self.channel
            .call(
                Event::from_string(event.to_string()),
                to_payload(payload)?,
                Duration::from_secs(30),
            )
            .await
            .map_err(|e| format!("{:?}", e))
            .and_then(|p| payload_to_value(&p).ok_or_else(|| "Invalid response".to_string()))
    }
}

fn create_hmac_signature(secret: &str, timestamp: i64, email: &str, api_key: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(format!("{}.{}.{}.{}", timestamp, email, api_key, "").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn to_payload(v: &Value) -> Result<Payload, String> {
    Payload::json_from_serialized(v.to_string()).map_err(|e| format!("Payload error: {:?}", e))
}

fn payload_to_value(p: &Payload) -> Option<Value> {
    match p {
        Payload::JSONPayload { json } => Some(Value::from(json.clone())),
        Payload::Binary { .. } => None,
    }
}

/// Parse a document from a JSON response
pub fn parse_document(v: &Value) -> Option<Document> {
    Some(Document {
        id: Uuid::parse_str(v.get("id")?.as_str()?).ok()?,
        user_id: v
            .get("user_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok()),
        content: v.get("content")?.clone(),
        sync_revision: v.get("sync_revision")?.as_i64()?,
        content_hash: v
            .get("content_hash")
            .and_then(|v| v.as_str())
            .map(String::from),
        title: v.get("title").and_then(|v| v.as_str()).map(String::from),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        deleted_at: None,
    })
}
