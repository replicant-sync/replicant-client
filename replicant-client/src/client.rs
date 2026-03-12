use crate::{database::ClientDatabase, events::EventDispatcher, websocket::WebSocketClient};
use replicant_core::{
    errors::ClientError,
    models::{Document, SyncStatus},
    patches::{apply_patch, create_patch},
    protocol::{ClientMessage, ServerMessage},
    SyncResult,
};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex, Notify};
use uuid::Uuid;

// Ping intervals for heartbeat detection
const PING_INTERVAL: Duration = Duration::from_secs(10); // Send ping every 10 seconds

#[derive(Debug, Clone)]
struct PendingUpload {
    operation_type: UploadType,
    sent_at: Instant,
}

#[derive(Debug, Clone)]
enum UploadType {
    Create,
    Update,
    Delete,
}

pub struct Client {
    db: Arc<ClientDatabase>,
    ws_client: Arc<Mutex<Option<WebSocketClient>>>,
    user_id: Uuid,
    client_id: Uuid,
    message_rx: Option<mpsc::Receiver<ServerMessage>>,
    event_dispatcher: Arc<EventDispatcher>,
    pending_uploads: Arc<Mutex<HashMap<Uuid, PendingUpload>>>,
    upload_complete_notifier: Arc<Notify>,
    sync_protection_mode: Arc<AtomicBool>,
    is_connected: Arc<AtomicBool>,
    last_ping_time: Arc<Mutex<Option<Instant>>>,
    server_url: String,
    email: String,
    api_key: String,
    api_secret: String,
    // Channel for triggering pending sync after reconnection
    reconnect_sync_tx: mpsc::Sender<()>,
    reconnect_sync_rx: Option<mpsc::Receiver<()>>,
    // Queue for deferred sync messages during upload protection
    deferred_messages: Arc<Mutex<Vec<ServerMessage>>>,
}

impl Client {
    pub async fn new(
        database_url: &str,
        server_url: &str,
        email: &str,
        api_key: &str,
        api_secret: &str,
    ) -> SyncResult<Self> {
        Self::with_event_dispatcher(database_url, server_url, email, api_key, api_secret, None)
            .await
    }

    pub async fn with_event_dispatcher(
        database_url: &str,
        server_url: &str,
        email: &str,
        api_key: &str,
        api_secret: &str,
        event_dispatcher: Option<Arc<EventDispatcher>>,
    ) -> SyncResult<Self> {
        let db = Arc::new(ClientDatabase::new(database_url).await?);
        db.run_migrations().await?;

        // Ensure user_config exists with deterministic user ID based on email
        db.ensure_user_config_with_identifier(server_url, email)
            .await?;

        let (user_id, client_id) = db.get_user_and_client_id().await?;

        let event_dispatcher = event_dispatcher.unwrap_or_else(|| Arc::new(EventDispatcher::new()));

        // Create a channel for messages
        let (tx, rx) = mpsc::channel(100);

        // Create a channel for reconnection sync triggers
        let (reconnect_sync_tx, reconnect_sync_rx) = mpsc::channel(10);

        let is_connected = Arc::new(AtomicBool::new(false));
        // Try to connect to WebSocket, but don't fail if offline
        let (ws_client, initial_ping_time) = match WebSocketClient::connect(
            server_url,
            email,
            client_id,
            user_id,
            api_key,
            api_secret,
            Some(event_dispatcher.clone()),
            is_connected.clone(),
        )
        .await
        {
            Ok((client, receiver)) => {
                // Start forwarding WebSocket messages to our channel
                tokio::spawn(async move {
                    if let Err(e) = receiver.forward_to(tx).await {
                        tracing::error!("WebSocket receiver error: {}", e);
                    }
                });
                (Some(client), Some(Instant::now()))
            }
            Err(e) => {
                eprintln!("Failed to connect to server (will retry): {}", e);
                (None, None)
            }
        };
        let mut engine = Self {
            db: db.clone(),
            ws_client: Arc::new(Mutex::new(ws_client)),
            user_id,
            client_id,
            message_rx: Some(rx),
            event_dispatcher: event_dispatcher.clone(),
            pending_uploads: Arc::new(Mutex::new(HashMap::new())),
            upload_complete_notifier: Arc::new(Notify::new()),
            sync_protection_mode: Arc::new(AtomicBool::new(false)),
            is_connected: is_connected,
            last_ping_time: Arc::new(Mutex::new(initial_ping_time)),
            server_url: server_url.to_string(),
            email: email.to_string(),
            api_key: api_key.to_string(),
            api_secret: api_secret.to_string(),
            reconnect_sync_tx,
            reconnect_sync_rx: Some(reconnect_sync_rx),
            deferred_messages: Arc::new(Mutex::new(Vec::new())),
        };

        // Automatically start background tasks
        engine.spawn_background_tasks().await?;

        Ok(engine)
    }

    pub fn event_dispatcher(&self) -> Arc<EventDispatcher> {
        self.event_dispatcher.clone()
    }

    async fn spawn_background_tasks(&mut self) -> SyncResult<()> {
        // Take the receiver - can only start once
        let rx = self
            .message_rx
            .take()
            .ok_or_else(|| ClientError::WebSocket("Client already started".to_string()))?;

        // Take the reconnect sync receiver
        let reconnect_sync_rx = self.reconnect_sync_rx.take().ok_or_else(|| {
            ClientError::WebSocket("Client reconnect sync already started".to_string())
        })?;

        let db = self.db.clone();
        let client_id = self.client_id;
        let event_dispatcher = self.event_dispatcher.clone();
        let pending_uploads = self.pending_uploads.clone();
        let upload_complete_notifier = self.upload_complete_notifier.clone();
        let sync_protection_mode = self.sync_protection_mode.clone();
        let ws_client = self.ws_client.clone();
        let deferred_messages = self.deferred_messages.clone();

        // Clone variables for the reconnection sync handler
        let db_for_reconnect_sync = db.clone();
        let pending_uploads_for_reconnect_sync = pending_uploads.clone();
        let ws_client_for_reconnect_sync = ws_client.clone();

        self.start_reconnection_loop();

        // Spawn message handler with upload tracking
        tokio::spawn(async move {
            let mut rx = rx;
            tracing::info!("CLIENT {}: Message handler started", client_id);
            while let Some(msg) = rx.recv().await {
                tracing::info!(
                    "CLIENT {}: Processing server message: {:?}",
                    client_id,
                    std::mem::discriminant(&msg)
                );
                if let Err(e) = Self::handle_server_message_with_tracking(
                    msg,
                    &db,
                    client_id,
                    &event_dispatcher,
                    &pending_uploads,
                    &upload_complete_notifier,
                    &sync_protection_mode,
                    &deferred_messages,
                )
                .await
                {
                    tracing::error!("CLIENT {}: Error handling server message: {}", client_id, e);
                } else {
                    tracing::info!(
                        "CLIENT {}: Successfully processed server message",
                        client_id
                    );
                }
            }
            tracing::warn!("CLIENT {}: Message handler terminated", client_id);
        });

        // Spawn reconnection sync handler
        tokio::spawn(async move {
            let mut reconnect_sync_rx = reconnect_sync_rx;
            tracing::info!("CLIENT {}: Reconnection sync handler started", client_id);

            #[allow(clippy::redundant_pattern_matching)] // Preserve drop order
            while let Some(_) = reconnect_sync_rx.recv().await {
                tracing::info!("CLIENT {}: Received reconnection sync trigger", client_id);

                // Perform pending sync using the actual engine components
                if let Err(e) = Self::perform_pending_sync_after_reconnection(
                    &db_for_reconnect_sync,
                    &ws_client_for_reconnect_sync,
                    client_id,
                    &pending_uploads_for_reconnect_sync,
                )
                .await
                {
                    tracing::error!(
                        "CLIENT {}: Failed to sync pending documents after reconnection: {}",
                        client_id,
                        e
                    );
                } else {
                    tracing::info!(
                        "✅ CLIENT {}: Pending documents sync completed after reconnection",
                        client_id
                    );

                    // NOW request full sync after uploads are complete
                    tracing::info!(
                        "🔄 CLIENT {}: Requesting full sync to get missed updates",
                        client_id
                    );
                    if let Some(client) = ws_client_for_reconnect_sync.lock().await.as_ref() {
                        if let Err(e) = client.send(ClientMessage::RequestFullSync).await {
                            tracing::error!(
                                "CLIENT {}: Failed to request full sync after reconnection: {}",
                                client_id,
                                e
                            );
                        } else {
                            tracing::info!(
                                "✅ CLIENT {}: RequestFullSync sent after pending uploads complete",
                                client_id
                            );
                        }
                    }
                }
            }

            tracing::warn!("CLIENT {}: Reconnection sync handler terminated", client_id);
        });

        // Only perform initial sync if connected
        if self.is_connected.load(Ordering::Relaxed) {
            // Upload-first strategy with protection
            self.event_dispatcher.emit_sync_started();

            // Enable protection mode during upload phase
            self.sync_protection_mode.store(true, Ordering::Relaxed);
            tracing::info!(
                "CLIENT {}: Protection mode ENABLED - blocking server overwrites during upload",
                self.client_id
            );

            // First: Upload any pending documents that were created/modified offline
            tracing::info!(
                "CLIENT {}: Starting upload-first sync - uploading pending changes",
                self.client_id
            );
            self.sync_pending_documents().await?;

            // Wait for upload confirmations with timeout
            if !self.pending_uploads.lock().await.is_empty() {
                let upload_count = self.pending_uploads.lock().await.len();
                tracing::info!(
                    "CLIENT {}: Waiting for {} upload confirmations",
                    self.client_id,
                    upload_count
                );

                tokio::select! {
                    _ = self.upload_complete_notifier.notified() => {
                        tracing::info!("CLIENT {}: All uploads confirmed successfully", self.client_id);
                    }
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(10)) => {
                        let remaining = self.pending_uploads.lock().await.len();
                        if remaining > 0 {
                            tracing::warn!("CLIENT {}: Upload timeout - {} uploads still pending", self.client_id, remaining);

                            // Enhanced fallback: Retry failed uploads before proceeding
                            tracing::info!("CLIENT {}: Retrying failed uploads before sync", self.client_id);
                            if let Err(e) = self.retry_failed_uploads().await {
                                tracing::error!("CLIENT {}: Retry failed: {}", self.client_id, e);
                            }
                        } else {
                            tracing::info!("CLIENT {}: Upload timeout but all uploads completed", self.client_id);
                        }
                    }
                }
            } else {
                tracing::info!("CLIENT {}: No pending uploads to wait for", self.client_id);
            }

            // Disable protection mode - now safe to receive server sync
            self.sync_protection_mode.store(false, Ordering::Relaxed);
            tracing::info!(
                "CLIENT {}: Protection mode DISABLED - server sync now allowed",
                self.client_id
            );

            // Process any deferred messages that were queued during upload phase
            if let Err(e) = Self::process_deferred_messages(
                &self.deferred_messages,
                &self.db,
                self.client_id,
                &self.event_dispatcher,
            )
            .await
            {
                tracing::error!(
                    "CLIENT {}: Error processing deferred messages: {}",
                    self.client_id,
                    e
                );
            }

            // Second: Download current server state (which now includes our uploaded documents)
            tracing::info!(
                "CLIENT {}: Upload phase complete, requesting server state",
                self.client_id
            );
            self.sync_all().await?;
        } else {
            tracing::info!(
                "CLIENT {}: Starting in offline mode - will sync when connection available",
                self.client_id
            );
        }

        Ok(())
    }

    pub async fn create_document(&self, content: serde_json::Value) -> SyncResult<Document> {
        self.create_document_with_id(Uuid::new_v4(), content).await
    }

    pub async fn create_document_with_id(
        &self,
        id: Uuid,
        content: serde_json::Value,
    ) -> SyncResult<Document> {
        let doc = Document {
            id,
            user_id: Some(self.user_id),
            content,
            sync_revision: 1,
            content_hash: None,
            title: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            deleted_at: None,
        };

        tracing::info!(
            "CLIENT {}: Creating document locally: {}",
            self.client_id,
            doc.id
        );
        self.db
            .save_document_with_status(&doc, Some(SyncStatus::Pending))
            .await?;

        self.event_dispatcher
            .emit_document_created(&doc.id, &doc.content);

        if let Err(e) = self.try_immediate_sync(&doc).await {
            tracing::warn!(
                "CLIENT {}: Failed to immediately sync new document {}: {}. Will retry later.",
                self.client_id,
                doc.id,
                e
            );
        }

        Ok(doc)
    }

    pub async fn update_document(
        &self,
        id: Uuid,
        new_content: serde_json::Value,
    ) -> SyncResult<()> {
        let mut doc = self.db.get_document(&id).await?;
        let old_content = doc.content.clone();
        let old_version = doc.sync_revision;

        tracing::info!("CLIENT {}: 📝 UPDATING DOCUMENT {}", self.client_id, id);
        tracing::info!(
            "CLIENT {}: OLD: content={:?}, version={}",
            self.client_id,
            old_content,
            old_version
        );
        tracing::info!("CLIENT {}: NEW: content={:?}", self.client_id, new_content);

        // Create patch for sync
        let patch = create_patch(&old_content, &new_content)?;

        // Update document
        doc.content = new_content.clone();
        // DON'T increment version locally - server is authoritative for versions
        // Server will increment atomically and broadcast back to all clients
        doc.content_hash = None; // Will be recalculated
        doc.updated_at = chrono::Utc::now();

        tracing::info!(
            "CLIENT {}: 💾 SAVING LOCALLY: version={}, marking as pending",
            self.client_id,
            doc.sync_revision
        );

        // CRITICAL: Atomically save document and queue patch
        // This prevents data loss if app crashes between operations
        use replicant_core::patches::calculate_checksum;
        use replicant_core::protocol::ChangeEventType;

        // Calculate hash of old content for optimistic locking
        let old_content_hash = calculate_checksum(&old_content);

        tracing::info!(
            "CLIENT {}: 📋 Atomically saving document and queueing patch for doc {}",
            self.client_id,
            doc.id
        );
        self.db
            .save_document_and_queue_patch(
                &doc,
                &patch,
                ChangeEventType::Update,
                Some(old_content_hash),
            )
            .await?;
        tracing::info!(
            "CLIENT {}: ✅ Successfully saved document and queued patch atomically",
            self.client_id
        );

        // Verify it was saved correctly and check its sync status
        let saved_doc = self.db.get_document(&id).await?;
        tracing::info!(
            "CLIENT {}: ✅ SAVED: content={:?}, version={}",
            self.client_id,
            saved_doc.content,
            saved_doc.sync_revision
        );

        // Check sync status after save
        let sync_status_result = sqlx::query("SELECT sync_status FROM documents WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(&self.db.pool)
            .await;

        match sync_status_result {
            Ok(row) => {
                let sync_status: String = row
                    .try_get("sync_status")
                    .unwrap_or_else(|_| "unknown".to_string());
                tracing::info!(
                    "CLIENT {}: 📊 Document {} sync_status after save: {}",
                    self.client_id,
                    id,
                    sync_status
                );
            }
            Err(e) => {
                tracing::error!(
                    "CLIENT {}: Failed to check sync_status: {}",
                    self.client_id,
                    e
                );
            }
        }

        // Emit event
        self.event_dispatcher
            .emit_document_updated(&doc.id, &doc.content);

        // Attempt immediate sync if connected
        tracing::info!(
            "CLIENT {}: 🚀 Attempting immediate sync for updated document {}",
            self.client_id,
            doc.id
        );
        if let Err(e) = self.try_immediate_sync(&doc).await {
            tracing::warn!("CLIENT {}: ⚠️  OFFLINE EDIT - Failed to immediately sync updated document {}: {}. Changes saved locally for later sync.", 
                         self.client_id, doc.id, e);
            // Document stays in "pending" status for next sync attempt

            // Double-check sync status after failed immediate sync
            let sync_status_result = sqlx::query("SELECT sync_status FROM documents WHERE id = ?")
                .bind(id.to_string())
                .fetch_one(&self.db.pool)
                .await;

            match sync_status_result {
                Ok(row) => {
                    let sync_status: String = row
                        .try_get("sync_status")
                        .unwrap_or_else(|_| "unknown".to_string());
                    tracing::warn!(
                        "CLIENT {}: 📊 Document {} sync_status after FAILED immediate sync: {}",
                        self.client_id,
                        id,
                        sync_status
                    );
                }
                Err(e) => {
                    tracing::error!(
                        "CLIENT {}: Failed to check sync_status after failed sync: {}",
                        self.client_id,
                        e
                    );
                }
            }
        } else {
            tracing::info!(
                "CLIENT {}: ✅ Immediate sync successful for document {}",
                self.client_id,
                doc.id
            );
        }

        Ok(())
    }

    pub async fn delete_document(&self, id: Uuid) -> SyncResult<()> {
        // Mark as deleted locally first
        self.db.delete_document(&id).await?;

        // Emit event
        self.event_dispatcher.emit_document_deleted(&id);

        // Try to send delete to server if connected
        let ws_client = self.ws_client.lock().await;
        if let Some(client) = ws_client.as_ref() {
            if let Err(e) = client
                .send(ClientMessage::DeleteDocument { document_id: id })
                .await
            {
                tracing::warn!(
                    "CLIENT {}: Failed to send delete to server: {}. Will sync later.",
                    self.client_id,
                    e
                );
                self.is_connected.store(false, Ordering::Relaxed);
                self.event_dispatcher.emit_connection_lost(&self.server_url);
                drop(ws_client);
                self.start_reconnection_loop();
            }
        } else {
            tracing::info!(
                "CLIENT {}: Offline - delete will sync when connection available",
                self.client_id
            );
        }

        Ok(())
    }

    pub async fn get_all_documents(&self) -> SyncResult<Vec<Document>> {
        let docs = self.db.get_all_documents().await?;
        tracing::info!(
            "CLIENT: get_all_documents() returning {} documents",
            docs.len()
        );
        for doc in &docs {
            tracing::info!(
                "CLIENT:   - Document: {} (updated: {})",
                doc.id,
                doc.updated_at
            );
        }
        Ok(docs)
    }

    pub async fn count_documents(&self) -> SyncResult<usize> {
        let docs = self.db.get_all_documents().await?;
        Ok(docs.len())
    }

    pub async fn count_pending_sync(&self) -> SyncResult<usize> {
        let pending_docs = self.db.get_pending_documents().await?;
        Ok(pending_docs.len())
    }

    async fn sync_pending_documents(&self) -> SyncResult<()> {
        let pending_docs = self.db.get_pending_documents().await?;
        // Also check sync_queue for debugging
        let sync_queue_result = sqlx::query("SELECT COUNT(*) as count FROM sync_queue")
            .fetch_one(&self.db.pool)
            .await;

        match sync_queue_result {
            Ok(row) => {
                let count: i64 = row.try_get("count").unwrap_or(0);
                tracing::info!(
                    "CLIENT {}: 📋 sync_queue contains {} entries",
                    self.client_id,
                    count
                );

                if count > 0 {
                    // Show what's in the sync_queue
                    let queue_entries = sqlx::query(
                        "SELECT document_id, operation_type, created_at FROM sync_queue",
                    )
                    .fetch_all(&self.db.pool)
                    .await;

                    match queue_entries {
                        Ok(rows) => {
                            for row in rows {
                                let doc_id: String = row
                                    .try_get("document_id")
                                    .unwrap_or_else(|_| "unknown".to_string());
                                let op_type: String = row
                                    .try_get("operation_type")
                                    .unwrap_or_else(|_| "unknown".to_string());
                                let created_at: String = row
                                    .try_get("created_at")
                                    .unwrap_or_else(|_| "unknown".to_string());
                                tracing::info!("CLIENT {}: 📋 sync_queue entry: doc_id={}, op_type={}, created_at={}", 
                                             self.client_id, doc_id, op_type, created_at);
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                "CLIENT {}: Failed to query sync_queue entries: {}",
                                self.client_id,
                                e
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    "CLIENT {}: Failed to count sync_queue: {}",
                    self.client_id,
                    e
                );
            }
        }

        if pending_docs.is_empty() {
            tracing::info!("CLIENT {}: No pending documents to sync", self.client_id);
            return Ok(());
        }

        tracing::info!(
            "CLIENT {}: 📤 UPLOADING {} PENDING DOCUMENTS",
            self.client_id,
            pending_docs.len()
        );

        // Show details of each pending document
        for (i, pending_info) in pending_docs.iter().enumerate() {
            if let Ok(doc) = self.db.get_document(&pending_info.id).await {
                tracing::info!(
                    "CLIENT {}: PENDING {}/{}: doc_id={}, content={:?}, version={}",
                    self.client_id,
                    i + 1,
                    pending_docs.len(),
                    pending_info.id,
                    doc.content,
                    doc.sync_revision
                );
            }
        }

        for pending_info in pending_docs {
            match self.db.get_document(&pending_info.id).await {
                Ok(doc) => {
                    let upload_type = if pending_info.is_deleted {
                        // Handle pending delete
                        tracing::info!(
                            "CLIENT {}: Uploading pending delete for doc {}",
                            self.client_id,
                            pending_info.id
                        );

                        // Track this upload
                        self.pending_uploads.lock().await.insert(
                            pending_info.id,
                            PendingUpload {
                                operation_type: UploadType::Delete,
                                sent_at: Instant::now(),
                            },
                        );

                        let ws_client = self.ws_client.lock().await;
                        if let Some(client) = ws_client.as_ref() {
                            client
                                .send(ClientMessage::DeleteDocument {
                                    document_id: pending_info.id,
                                })
                                .await?;
                        } else {
                            return Err(ClientError::WebSocket("Not connected".to_string()))?;
                        }

                        UploadType::Delete
                    } else {
                        // Check if we have a patch stored in sync_queue to determine if this is create or update
                        // With server-authoritative versioning, we can't rely on version number anymore
                        match self.db.get_queued_patch(&pending_info.id).await? {
                            Some((stored_patch, old_hash_opt)) => {
                                // Have a queued patch = this is an UPDATE
                                tracing::info!(
                                    "CLIENT {}: 📋 Found stored patch in sync_queue for doc {} - treating as UPDATE",
                                    self.client_id,
                                    pending_info.id
                                );

                                // Track this upload
                                self.pending_uploads.lock().await.insert(
                                    pending_info.id,
                                    PendingUpload {
                                        operation_type: UploadType::Update,
                                        sent_at: Instant::now(),
                                    },
                                );

                                // Use the stored patch for UpdateDocument
                                use replicant_core::models::DocumentPatch;
                                use replicant_core::patches::calculate_checksum;

                                let content_hash = old_hash_opt
                                    .unwrap_or_else(|| calculate_checksum(&doc.content));

                                let document_patch = DocumentPatch {
                                    document_id: pending_info.id,
                                    patch: stored_patch,
                                    content_hash,
                                };

                                let ws_client = self.ws_client.lock().await;
                                if let Some(client) = ws_client.as_ref() {
                                    tracing::info!(
                                        "CLIENT {}: ✅ Sending UpdateDocument with stored patch",
                                        self.client_id
                                    );
                                    client
                                        .send(ClientMessage::UpdateDocument {
                                            patch: document_patch,
                                        })
                                        .await?;
                                } else {
                                    return Err(ClientError::WebSocket(
                                        "Not connected".to_string(),
                                    ))?;
                                }

                                UploadType::Update
                            }
                            None => {
                                // No queued patch = this is a CREATE
                                tracing::info!(
                                    "CLIENT {}: No queued patch found for doc {} - treating as CREATE",
                                    self.client_id,
                                    pending_info.id
                                );

                                // Track this upload
                                self.pending_uploads.lock().await.insert(
                                    pending_info.id,
                                    PendingUpload {
                                        operation_type: UploadType::Create,
                                        sent_at: Instant::now(),
                                    },
                                );

                                let ws_client = self.ws_client.lock().await;
                                if let Some(client) = ws_client.as_ref() {
                                    client
                                        .send(ClientMessage::CreateDocument {
                                            document: doc.clone(),
                                        })
                                        .await?;
                                } else {
                                    return Err(ClientError::WebSocket(
                                        "Not connected".to_string(),
                                    ))?;
                                }

                                UploadType::Create
                            }
                        }
                    };

                    tracing::debug!(
                        "CLIENT {}: Tracked upload for document {} ({:?})",
                        self.client_id,
                        pending_info.id,
                        upload_type
                    );
                }
                Err(e) => {
                    tracing::error!(
                        "CLIENT {}: Failed to get pending document {}: {}",
                        self.client_id,
                        pending_info.id,
                        e
                    );
                }
            }
        }

        tracing::info!(
            "CLIENT {}: Upload tracking: {} operations pending confirmation",
            self.client_id,
            self.pending_uploads.lock().await.len()
        );
        Ok(())
    }

    // Enhanced message handler with upload tracking and protection
    async fn handle_server_message_with_tracking(
        msg: ServerMessage,
        db: &Arc<ClientDatabase>,
        client_id: Uuid,
        event_dispatcher: &Arc<EventDispatcher>,
        pending_uploads: &Arc<Mutex<HashMap<Uuid, PendingUpload>>>,
        upload_complete_notifier: &Arc<Notify>,
        sync_protection_mode: &Arc<AtomicBool>,
        deferred_messages: &Arc<Mutex<Vec<ServerMessage>>>,
    ) -> SyncResult<()> {
        match &msg {
            // Handle upload confirmations first
            ServerMessage::DocumentCreatedResponse {
                document_id,
                success,
                ..
            }
            | ServerMessage::DocumentUpdatedResponse {
                document_id,
                success,
                ..
            }
            | ServerMessage::DocumentDeletedResponse {
                document_id,
                success,
                ..
            } => {
                if *success {
                    // Remove from pending uploads
                    let mut uploads = pending_uploads.lock().await;
                    if let Some(upload) = uploads.remove(document_id) {
                        let elapsed = upload.sent_at.elapsed();
                        tracing::info!(
                            "CLIENT {}: Upload confirmed for {} ({:?}) in {:?}",
                            client_id,
                            document_id,
                            upload.operation_type,
                            elapsed
                        );

                        // If this was the last pending upload, notify
                        if uploads.is_empty() {
                            tracing::info!(
                                "CLIENT {}: All uploads confirmed - notifying completion",
                                client_id
                            );
                            upload_complete_notifier.notify_one();
                        }
                    }
                    // Release the lock before processing deferred messages
                    drop(uploads);

                    // Process any deferred messages now that this upload is complete
                    // Some messages might have been queued while this document was uploading
                    if let Err(e) = Self::process_deferred_messages(
                        deferred_messages,
                        db,
                        client_id,
                        event_dispatcher,
                    )
                    .await
                    {
                        tracing::error!(
                            "CLIENT {}: Error processing deferred messages after upload: {}",
                            client_id,
                            e
                        );
                    }
                } else {
                    tracing::error!(
                        "CLIENT {}: Upload failed for document {}",
                        client_id,
                        document_id
                    );
                }

                // Continue with normal processing
                return Self::handle_server_message(msg, db, client_id, event_dispatcher).await;
            }

            // Apply protection for sync messages during upload phase
            ServerMessage::SyncDocument { document } => {
                // Check if we're in protection mode
                if sync_protection_mode.load(Ordering::Relaxed) {
                    tracing::info!(
                        "CLIENT {}: 🔒 QUEUEING sync for {} v{} (protection mode active)",
                        client_id,
                        document.id,
                        document.sync_revision
                    );
                    // Queue message for later processing instead of dropping it
                    const MAX_DEFERRED_MESSAGES: usize = 100;
                    let mut queue = deferred_messages.lock().await;
                    if queue.len() >= MAX_DEFERRED_MESSAGES {
                        tracing::warn!(
                            "CLIENT {}: Deferred queue full ({} messages), dropping oldest",
                            client_id,
                            queue.len()
                        );
                        queue.remove(0);
                    }
                    queue.push(ServerMessage::SyncDocument {
                        document: document.clone(),
                    });
                    return Ok(());
                }

                // Check if document has pending changes
                if let Ok(_local_doc) = db.get_document(&document.id).await {
                    // Check if this document has an active upload in progress
                    // This is our primary protection mechanism
                    if Self::has_pending_upload(pending_uploads, &document.id).await {
                        tracing::info!(
                            "CLIENT {}: 🔒 QUEUEING sync for {} v{} (upload in progress)",
                            client_id,
                            document.id,
                            document.sync_revision
                        );
                        // Queue message for later processing instead of dropping it
                        const MAX_DEFERRED_MESSAGES: usize = 100;
                        let mut queue = deferred_messages.lock().await;
                        if queue.len() >= MAX_DEFERRED_MESSAGES {
                            tracing::warn!(
                                "CLIENT {}: Deferred queue full ({} messages), dropping oldest",
                                client_id,
                                queue.len()
                            );
                            queue.remove(0);
                        }
                        queue.push(ServerMessage::SyncDocument {
                            document: document.clone(),
                        });
                        return Ok(());
                    }
                }

                // Safe to proceed with sync
                return Self::handle_server_message(msg, db, client_id, event_dispatcher).await;
            }

            _ => {
                // For all other messages, use normal handling
                return Self::handle_server_message(msg, db, client_id, event_dispatcher).await;
            }
        }
    }

    /// Process all deferred sync messages that were queued during upload protection
    async fn process_deferred_messages(
        deferred_messages: &Arc<Mutex<Vec<ServerMessage>>>,
        db: &Arc<ClientDatabase>,
        client_id: Uuid,
        event_dispatcher: &Arc<EventDispatcher>,
    ) -> SyncResult<()> {
        let mut messages = deferred_messages.lock().await;
        let count = messages.len();

        if count == 0 {
            return Ok(());
        }

        tracing::info!(
            "CLIENT {}: Processing {} deferred sync messages",
            client_id,
            count
        );

        // Process all deferred messages in order
        for msg in messages.drain(..) {
            if let Err(e) = Self::handle_server_message(msg, db, client_id, event_dispatcher).await
            {
                tracing::error!(
                    "CLIENT {}: Error processing deferred message: {}",
                    client_id,
                    e
                );
                // Continue processing remaining messages even if one fails
            }
        }

        tracing::info!(
            "CLIENT {}: Completed processing deferred messages",
            client_id
        );

        Ok(())
    }

    // Check if a document has an active upload in progress
    async fn has_pending_upload(
        pending_uploads: &Arc<Mutex<HashMap<Uuid, PendingUpload>>>,
        document_id: &Uuid,
    ) -> bool {
        let uploads = pending_uploads.lock().await;
        uploads.contains_key(document_id)
    }

    async fn handle_server_message(
        msg: ServerMessage,
        db: &Arc<ClientDatabase>,
        client_id: Uuid,
        event_dispatcher: &Arc<EventDispatcher>,
    ) -> SyncResult<()> {
        match msg {
            ServerMessage::DocumentUpdated { patch } => {
                // Apply patch from server
                tracing::info!(
                    "CLIENT {}: Received DocumentUpdated for doc {}",
                    client_id,
                    patch.document_id
                );
                let mut doc = db.get_document(&patch.document_id).await?;

                tracing::info!(
                    "CLIENT {}: Document content before patch: {:?}",
                    client_id,
                    doc.content
                );
                tracing::info!("CLIENT {}: Patch to apply: {:?}", client_id, patch.patch);

                // Apply patch
                apply_patch(&mut doc.content, &patch.patch)?;
                doc.content_hash = None; // Will be recalculated
                doc.updated_at = chrono::Utc::now();

                tracing::info!(
                    "CLIENT {}: Document content after patch: {:?}",
                    client_id,
                    doc.content
                );

                db.save_document(&doc).await?;
                db.mark_synced(&doc.id).await?;

                // Emit event for updated document
                event_dispatcher.emit_document_updated(&doc.id, &doc.content);
            }
            ServerMessage::DocumentCreated { document } => {
                // New document from server - check if we already have it to avoid duplicates
                tracing::info!(
                    "CLIENT: Received DocumentCreated from server: {}",
                    document.id
                );

                // Check if we already have this document (e.g., if we were the creator)
                match db.get_document(&document.id).await {
                    Ok(existing_doc) => {
                        // We already have this document - just ensure it's marked as synced
                        if existing_doc.sync_revision == document.sync_revision {
                            tracing::info!("CLIENT: Document {} already exists locally with same sync_revision, marking as synced", document.id);
                            db.mark_synced(&document.id).await?;
                        } else {
                            // Different revision - update it
                            tracing::info!("CLIENT: Document {} exists locally but has different revision, updating", document.id);
                            db.save_document_with_status(&document, Some(SyncStatus::Synced))
                                .await?;

                            // Emit event for updated document
                            event_dispatcher.emit_document_updated(&document.id, &document.content);
                        }
                    }
                    Err(_) => {
                        // Document doesn't exist locally - save it
                        tracing::info!(
                            "CLIENT: Document {} is new, saving to local database",
                            document.id
                        );
                        db.save_document_with_status(&document, Some(SyncStatus::Synced))
                            .await?;

                        // Emit event for new document from server
                        event_dispatcher.emit_document_created(&document.id, &document.content);
                    }
                }
            }
            ServerMessage::DocumentDeleted { document_id } => {
                // Document deleted from server - we need to delete it locally
                tracing::info!(
                    "CLIENT {}: Received DocumentDeleted for doc {}",
                    client_id,
                    document_id
                );

                // Delete the document locally (soft delete)
                db.delete_document(&document_id).await?;

                // Mark it as synced so we don't try to sync the delete again
                db.mark_synced(&document_id).await?;

                // Emit event for deleted document
                event_dispatcher.emit_document_deleted(&document_id);
            }
            ServerMessage::ConflictDetected { document_id, .. } => {
                tracing::warn!("Conflict detected for document {}", document_id);

                // Emit conflict event
                event_dispatcher.emit_conflict_detected(&document_id);
            }
            ServerMessage::SyncDocument { document } => {
                // Document sync - check if it's newer than what we have
                tracing::info!(
                    "CLIENT {}: 📥 RECEIVED SyncDocument: {} (sync_revision: {})",
                    client_id,
                    document.id,
                    document.sync_revision
                );

                match db.get_document(&document.id).await {
                    Ok(local_doc) => {
                        tracing::info!(
                            "CLIENT {}: LOCAL  DOCUMENT: content={:?}, version={}",
                            client_id,
                            local_doc.content,
                            local_doc.sync_revision
                        );
                        tracing::info!(
                            "CLIENT {}: SERVER DOCUMENT: content={:?}, version={}",
                            client_id,
                            document.content,
                            document.sync_revision
                        );

                        // Compare versions - server wins if version is >= local
                        // This handles the case where server broadcasts back our own update
                        let should_update = document.sync_revision >= local_doc.sync_revision;

                        tracing::info!(
                            "CLIENT {}: 📊 VERSION COMPARISON for doc {}: server v{} vs local v{} → should_update={}",
                            client_id,
                            document.id,
                            document.sync_revision,
                            local_doc.sync_revision,
                            should_update
                        );

                        if should_update {
                            // Check if this might be overwriting local changes by comparing content
                            if local_doc.content != document.content {
                                tracing::warn!(
                                    "CLIENT {}: ⚠️  SERVER OVERWRITING LOCAL CHANGES!",
                                    client_id
                                );
                                tracing::warn!(
                                    "CLIENT {}: LOCAL: {:?} → SERVER: {:?}",
                                    client_id,
                                    local_doc.content,
                                    document.content
                                );
                            }

                            tracing::info!(
                                "CLIENT {}: 🔄 Updating to newer version ({} -> {})",
                                client_id,
                                local_doc.sync_revision,
                                document.sync_revision
                            );
                            db.save_document_with_status(&document, Some(SyncStatus::Synced))
                                .await?;

                            // Emit event for updated document
                            event_dispatcher.emit_document_updated(&document.id, &document.content);
                        } else {
                            tracing::info!(
                                "CLIENT {}: Skipping older sync (local version {} >= sync version {})",
                                client_id,
                                local_doc.sync_revision,
                                document.sync_revision
                            );
                        }
                    }
                    Err(_) => {
                        // Document doesn't exist locally - save it
                        tracing::info!(
                            "CLIENT {}: Document {} is new, saving",
                            client_id,
                            document.id
                        );
                        db.save_document_with_status(&document, Some(SyncStatus::Synced))
                            .await?;

                        // Emit event for new document
                        event_dispatcher.emit_document_created(&document.id, &document.content);
                    }
                }
            }
            ServerMessage::SyncComplete { synced_count } => {
                tracing::debug!("Sync complete, received {} documents", synced_count);

                // Emit sync completed event
                event_dispatcher.emit_sync_completed(synced_count as u64);
            }

            // Handle document operation confirmations
            ServerMessage::DocumentCreatedResponse {
                document_id,
                success,
                error,
            } => {
                if success {
                    tracing::info!(
                        "CLIENT {}: Document creation confirmed by server: {}",
                        client_id,
                        document_id
                    );
                    db.mark_synced(&document_id).await?;
                    // Clean up sync_queue
                    db.remove_from_sync_queue(&document_id).await?;
                } else {
                    tracing::error!(
                        "CLIENT {}: Document creation failed on server: {} - {}",
                        client_id,
                        document_id,
                        error.as_deref().unwrap_or("unknown error")
                    );
                    // Could emit an error event here
                    event_dispatcher.emit_sync_error(&format!(
                        "Create failed: {}",
                        error.as_deref().unwrap_or("unknown")
                    ));
                }
            }

            ServerMessage::DocumentUpdatedResponse {
                document_id,
                success,
                error,
                sync_revision,
            } => {
                if success {
                    tracing::info!(
                        "CLIENT {}: Document update confirmed by server: {}",
                        client_id,
                        document_id
                    );
                    // Update local sync_revision if provided by server
                    if let Some(new_revision) = sync_revision {
                        tracing::info!(
                            "CLIENT {}: Updating local sync_revision to {} for doc {}",
                            client_id,
                            new_revision,
                            document_id
                        );
                        db.update_sync_revision(&document_id, new_revision).await?;
                    }
                    db.mark_synced(&document_id).await?;
                    // Clean up sync_queue
                    db.remove_from_sync_queue(&document_id).await?;
                    tracing::info!(
                        "CLIENT {}: Removed doc {} from sync_queue",
                        client_id,
                        document_id
                    );
                } else {
                    tracing::error!(
                        "CLIENT {}: Document update failed on server: {} - {}",
                        client_id,
                        document_id,
                        error.as_deref().unwrap_or("unknown error")
                    );
                    event_dispatcher.emit_sync_error(&format!(
                        "Update failed: {}",
                        error.as_deref().unwrap_or("unknown")
                    ));
                }
            }

            ServerMessage::DocumentDeletedResponse {
                document_id,
                success,
                error,
            } => {
                if success {
                    tracing::info!(
                        "CLIENT {}: Document deletion confirmed by server: {}",
                        client_id,
                        document_id
                    );
                    db.mark_synced(&document_id).await?;
                    // Clean up sync_queue
                    db.remove_from_sync_queue(&document_id).await?;
                } else {
                    tracing::error!(
                        "CLIENT {}: Document deletion failed on server: {} - {}",
                        client_id,
                        document_id,
                        error.as_deref().unwrap_or("unknown error")
                    );
                    event_dispatcher.emit_sync_error(&format!(
                        "Delete failed: {}",
                        error.as_deref().unwrap_or("unknown")
                    ));
                }
            }

            _ => {}
        }

        Ok(())
    }

    // Retry failed uploads by re-checking pending documents
    async fn retry_failed_uploads(&self) -> SyncResult<()> {
        tracing::info!(
            "CLIENT {}: Starting upload retry for failed operations",
            self.client_id
        );

        // Get current pending uploads (these are the ones that timed out)
        let timed_out_uploads: Vec<Uuid> = {
            let uploads = self.pending_uploads.lock().await;
            uploads.keys().cloned().collect()
        };

        if timed_out_uploads.is_empty() {
            tracing::info!("CLIENT {}: No timed out uploads to retry", self.client_id);
            return Ok(());
        }

        tracing::info!(
            "CLIENT {}: Retrying {} timed out uploads",
            self.client_id,
            timed_out_uploads.len()
        );

        // Clear the pending uploads (we'll re-add them during retry)
        self.pending_uploads.lock().await.clear();

        // Re-run sync_pending_documents to retry uploads
        // This will re-query the database for documents with pending status
        // and re-upload them with fresh tracking
        self.sync_pending_documents().await?;

        // Quick wait for the retry confirmations (shorter timeout)
        if !self.pending_uploads.lock().await.is_empty() {
            let retry_count = self.pending_uploads.lock().await.len();
            tracing::info!(
                "CLIENT {}: Waiting for {} retry confirmations (short timeout)",
                self.client_id,
                retry_count
            );

            tokio::select! {
                _ = self.upload_complete_notifier.notified() => {
                    tracing::info!("CLIENT {}: All retry uploads confirmed", self.client_id);
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                    let remaining = self.pending_uploads.lock().await.len();
                    tracing::warn!("CLIENT {}: Retry timeout - {} uploads still failing", self.client_id, remaining);
                    // Don't retry again - proceed with partial failure
                }
            }
        }

        Ok(())
    }

    pub async fn sync_all(&self) -> SyncResult<()> {
        // Request full sync on startup to get all documents
        tracing::debug!("Requesting full sync from server");

        let ws_client = self.ws_client.lock().await;
        if let Some(client) = ws_client.as_ref() {
            client.send(ClientMessage::RequestFullSync).await?;
        } else {
            tracing::warn!("CLIENT {}: Cannot sync - not connected", self.client_id);
            return Err(ClientError::WebSocket("Not connected".to_string()))?;
        }

        Ok(())
    }

    /// Check if the WebSocket connection is active
    pub fn is_connected(&self) -> bool {
        self.is_connected.load(Ordering::Relaxed)
    }

    /// Attempt to sync a single document immediately if connected
    async fn try_immediate_sync(&self, document: &Document) -> SyncResult<()> {
        let connected = self.is_connected();
        tracing::info!(
            "CLIENT {}: 🔍 Connection status check: connected={}",
            self.client_id,
            connected
        );

        if !connected {
            tracing::warn!("CLIENT {}: 📴 OFFLINE - Document {} cannot sync immediately, returning error to mark as pending", 
                         self.client_id, document.id);
            return Err(ClientError::WebSocket(
                "Client is offline - document should remain pending".to_string(),
            ))?;
        }

        tracing::info!(
            "CLIENT {}: 🚀 IMMEDIATE SYNC attempt for document {}",
            self.client_id,
            document.id
        );
        tracing::info!(
            "CLIENT {}: Document sync_revision: {}, content: {:?}",
            self.client_id,
            document.sync_revision,
            document.content
        );

        // Determine if this is create or update by checking for queued patch
        // If we have a queued patch, it's an update. Otherwise, it's a create.
        // This works correctly with server-authoritative versioning where client
        // doesn't increment version locally.
        let (operation_type, message) = match self.db.get_queued_patch(&document.id).await {
            Ok(Some((patch, old_hash_opt))) => {
                // Have a queued patch = this is an UPDATE
                tracing::info!(
                    "CLIENT {}: Sending UPDATE with queued patch for doc {}",
                    self.client_id,
                    document.id
                );

                use replicant_core::models::DocumentPatch;
                use replicant_core::patches::calculate_checksum;

                // Use the stored old content hash, or calculate from current content as fallback
                let content_hash =
                    old_hash_opt.unwrap_or_else(|| calculate_checksum(&document.content));

                (
                    UploadType::Update,
                    ClientMessage::UpdateDocument {
                        patch: DocumentPatch {
                            document_id: document.id,
                            patch,
                            content_hash,
                        },
                    },
                )
            }
            Ok(None) => {
                // No queued patch = this is a CREATE
                tracing::info!(
                    "CLIENT {}: Sending CREATE for doc {} (no queued patch found)",
                    self.client_id,
                    document.id
                );
                (
                    UploadType::Create,
                    ClientMessage::CreateDocument {
                        document: document.clone(),
                    },
                )
            }
            Err(e) => {
                // Error querying patch = this is a CREATE
                tracing::warn!(
                    "CLIENT {}: Sending CREATE for doc {} (error getting queued patch: {})",
                    self.client_id,
                    document.id,
                    e
                );
                (
                    UploadType::Create,
                    ClientMessage::CreateDocument {
                        document: document.clone(),
                    },
                )
            }
        };

        // Add to pending uploads for tracking
        {
            let mut uploads = self.pending_uploads.lock().await;
            uploads.insert(
                document.id,
                PendingUpload {
                    operation_type,
                    sent_at: Instant::now(),
                },
            );
        }

        let ws_client = self.ws_client.lock().await;
        match ws_client.as_ref() {
            Some(client) => {
                match client.send(message).await {
                    Ok(_) => {
                        tracing::info!(
                            "CLIENT {}: ✅ Immediate sync request sent for document {}",
                            self.client_id,
                            document.id
                        );
                        Ok(())
                    }
                    Err(e) => {
                        // Connection failed - mark as disconnected and remove from pending uploads
                        self.is_connected.store(false, Ordering::Relaxed);
                        self.event_dispatcher.emit_connection_lost(&self.server_url);
                        {
                            let mut uploads = self.pending_uploads.lock().await;
                            uploads.remove(&document.id);
                        }
                        tracing::warn!(
                            "CLIENT {}: WebSocket send failed, marked as disconnected",
                            self.client_id
                        );
                        // Start reconnection loop if not already running
                        drop(ws_client); // Release lock before starting reconnection
                        self.start_reconnection_loop();
                        Err(e)
                    }
                }
            }
            None => {
                tracing::warn!(
                    "CLIENT {}: No WebSocket connection available for immediate sync",
                    self.client_id
                );
                {
                    let mut uploads = self.pending_uploads.lock().await;
                    uploads.remove(&document.id);
                }
                Err(ClientError::WebSocket("Not connected".to_string()))?
            }
        }
    }

    /// Start the reconnection loop if not already running
    fn start_reconnection_loop(&self) {
        let is_connected = self.is_connected.clone();
        let ws_client = self.ws_client.clone();
        let server_url = self.server_url.clone();
        let email = self.email.clone();
        let api_key = self.api_key.clone();
        let api_secret = self.api_secret.clone();
        let client_id = self.client_id;
        let user_id = self.user_id;
        let event_dispatcher = self.event_dispatcher.clone();
        let db = self.db.clone();
        let pending_uploads = self.pending_uploads.clone();
        let upload_complete_notifier = self.upload_complete_notifier.clone();
        let reconnect_sync_tx = self.reconnect_sync_tx.clone();
        let sync_protection_mode = self.sync_protection_mode.clone();
        let last_ping_time = self.last_ping_time.clone();
        let deferred_messages = self.deferred_messages.clone();

        tracing::info!(
            "🔄 CLIENT {}: Starting continuous reconnection monitor (5-second intervals)",
            client_id
        );

        tokio::spawn(async move {
            const RECONNECTION_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
            let mut connection_attempts = 0;

            loop {
                let currently_connected = is_connected.load(Ordering::Relaxed);

                if !currently_connected {
                    connection_attempts += 1;
                    tracing::info!(
                        "🔌 CLIENT {}: Connection attempt #{} to {}",
                        client_id,
                        connection_attempts,
                        server_url
                    );

                    // Try to connect
                    match WebSocketClient::connect(
                        &server_url,
                        &email,
                        client_id,
                        user_id,
                        &api_key,
                        &api_secret,
                        Some(event_dispatcher.clone()),
                        is_connected.clone(),
                    )
                    .await
                    {
                        Ok((new_client, receiver)) => {
                            tracing::info!(
                                "✅ CLIENT {}: Reconnection successful after {} attempts!",
                                client_id,
                                connection_attempts
                            );
                            connection_attempts = 0;

                            // Update the client
                            *ws_client.lock().await = Some(new_client);
                            is_connected.store(true, Ordering::Relaxed);

                            // Reset ping timer on successful connection
                            *last_ping_time.lock().await = Some(Instant::now());

                            // Emit connection event
                            event_dispatcher.emit_connection_succeeded(&server_url);

                            // Start message receiver forwarding with connection monitoring
                            let (tx, mut rx) = mpsc::channel(100);
                            let receiver_is_connected = is_connected.clone();
                            let receiver_client_id = client_id;
                            let receiver_event_dispatcher = event_dispatcher.clone();
                            let receiver_server_url = server_url.clone();
                            tokio::spawn(async move {
                                match receiver.forward_to(tx).await {
                                    Ok(_) => {
                                        tracing::info!(
                                            "🔌 CLIENT {}: WebSocket receiver completed normally",
                                            receiver_client_id
                                        );
                                    }
                                    Err(e) => {
                                        tracing::warn!("❌ CLIENT {}: WebSocket receiver error: {} - marking as disconnected", receiver_client_id, e);
                                        receiver_is_connected.store(false, Ordering::Relaxed);
                                        receiver_event_dispatcher
                                            .emit_connection_lost(&receiver_server_url);
                                    }
                                }
                            });

                            // Process messages in background with connection monitoring
                            let db_clone = db.clone();
                            let event_dispatcher_clone = event_dispatcher.clone();
                            let pending_uploads_clone = pending_uploads.clone();
                            let upload_complete_notifier_clone = upload_complete_notifier.clone();
                            let sync_protection_mode_clone = sync_protection_mode.clone();
                            let deferred_messages_clone = deferred_messages.clone();
                            let handler_is_connected = is_connected.clone();
                            let handler_client_id = client_id;
                            let handler_server_url = server_url.clone();
                            tokio::spawn(async move {
                                while let Some(msg) = rx.recv().await {
                                    if let Err(e) = Self::handle_server_message_with_tracking(
                                        msg,
                                        &db_clone,
                                        handler_client_id,
                                        &event_dispatcher_clone,
                                        &pending_uploads_clone,
                                        &upload_complete_notifier_clone,
                                        &sync_protection_mode_clone,
                                        &deferred_messages_clone,
                                    )
                                    .await
                                    {
                                        tracing::error!(
                                            "CLIENT {}: Error handling server message: {}",
                                            handler_client_id,
                                            e
                                        );
                                    }
                                }
                                tracing::warn!("📪 CLIENT {}: Message handler terminated - marking as disconnected", handler_client_id);
                                handler_is_connected.store(false, Ordering::Relaxed);
                                event_dispatcher_clone.emit_connection_lost(&handler_server_url);
                            });

                            // Clear any stale pending uploads from before disconnection
                            // These are invalid now and will be re-uploaded if needed
                            {
                                let mut uploads = pending_uploads.lock().await;
                                if !uploads.is_empty() {
                                    tracing::info!("CLIENT {}: Clearing {} stale pending uploads from before reconnection",
                                                 client_id, uploads.len());
                                    uploads.clear();
                                }
                            }

                            // Trigger pending sync on the real sync engine via channel
                            // This will upload any pending documents and THEN request full sync
                            tracing::info!(
                                "📤 CLIENT {}: Triggering post-reconnection sync on real engine",
                                client_id
                            );

                            if let Err(e) = reconnect_sync_tx.try_send(()) {
                                tracing::error!(
                                    "CLIENT {}: Failed to trigger reconnection sync: {}",
                                    client_id,
                                    e
                                );
                            } else {
                                tracing::info!(
                                    "✅ CLIENT {}: Reconnection sync trigger sent to real engine",
                                    client_id
                                );
                            }
                            // The pending sync handler will request full sync after uploads complete
                        }
                        Err(e) => {
                            tracing::debug!("❌ CLIENT {}: Connection attempt #{} failed: {} - will retry in {}s", client_id, connection_attempts, e, RECONNECTION_INTERVAL.as_secs());
                            event_dispatcher.emit_connection_attempted(&server_url);
                        }
                    }
                } else {
                    // Connection is supposedly active - perform heartbeat check
                    let mut should_ping = false;
                    {
                        let last_ping = last_ping_time.lock().await;
                        match *last_ping {
                            Some(last_time) => {
                                if last_time.elapsed() >= PING_INTERVAL {
                                    should_ping = true;
                                }
                            }
                            None => {
                                // Never pinged before, time to start
                                should_ping = true;
                            }
                        }
                    }

                    if should_ping {
                        // Try to send a ping to verify connection is alive
                        tracing::info!(
                            "💓 CLIENT {}: Sending heartbeat ping to verify connection",
                            client_id
                        );
                        let client_guard = ws_client.lock().await;
                        match client_guard.as_ref() {
                            Some(client) => {
                                match client.send(ClientMessage::Ping).await {
                                    Ok(_) => {
                                        // Ping successful, update last ping time
                                        *last_ping_time.lock().await = Some(Instant::now());
                                        tracing::info!("✅ CLIENT {}: Heartbeat ping successful - connection alive", client_id);
                                    }
                                    Err(e) => {
                                        // Ping failed - connection is broken
                                        tracing::error!("💥 CLIENT {}: Heartbeat ping FAILED: {} - marking as disconnected and starting reconnection", client_id, e);
                                        is_connected.store(false, Ordering::Relaxed);
                                        event_dispatcher.emit_connection_lost(&server_url);
                                    }
                                }
                            }
                            None => {
                                // No client but connection flag says connected - inconsistent state
                                tracing::error!("⚠️ CLIENT {}: Connection flag says connected but no client found - marking as disconnected", client_id);
                                is_connected.store(false, Ordering::Relaxed);
                                event_dispatcher.emit_connection_lost(&server_url);
                            }
                        }
                    } else {
                        // Not time to ping yet
                        let last_ping = last_ping_time.lock().await;
                        match *last_ping {
                            Some(last_time) => {
                                let elapsed = last_time.elapsed();
                                tracing::debug!("💤 CLIENT {}: Heartbeat check - last ping was {:.1}s ago (will ping in {:.1}s)", 
                                    client_id, elapsed.as_secs_f32(), (PING_INTERVAL - elapsed).as_secs_f32());
                            }
                            None => {
                                tracing::debug!(
                                    "💤 CLIENT {}: Heartbeat check - no ping sent yet",
                                    client_id
                                );
                            }
                        }
                    }
                }

                // Wait before next check/retry
                tokio::time::sleep(RECONNECTION_INTERVAL).await;
            }
        });
    }

    /// Static method to perform pending sync after reconnection
    /// This is called from the reconnection loop and operates on real engine components
    async fn perform_pending_sync_after_reconnection(
        db: &Arc<ClientDatabase>,
        ws_client: &Arc<Mutex<Option<WebSocketClient>>>,
        client_id: Uuid,
        pending_uploads: &Arc<Mutex<HashMap<Uuid, PendingUpload>>>,
    ) -> SyncResult<()> {
        tracing::info!(
            "CLIENT {}: Starting post-reconnection pending sync using real engine components",
            client_id
        );

        let pending_docs = db.get_pending_documents().await?;

        if pending_docs.is_empty() {
            tracing::info!(
                "CLIENT {}: No pending documents to sync after reconnection",
                client_id
            );
            return Ok(());
        }

        tracing::info!(
            "CLIENT {}: 📤 UPLOADING {} PENDING DOCUMENTS after reconnection",
            client_id,
            pending_docs.len()
        );

        for pending_info in pending_docs {
            match db.get_document(&pending_info.id).await {
                Ok(doc) => {
                    if pending_info.is_deleted {
                        // Handle pending delete
                        tracing::info!(
                            "CLIENT {}: Uploading pending delete for doc {}",
                            client_id,
                            pending_info.id
                        );

                        // Track this upload
                        pending_uploads.lock().await.insert(
                            pending_info.id,
                            PendingUpload {
                                operation_type: UploadType::Delete,
                                sent_at: Instant::now(),
                            },
                        );

                        let ws_client_guard = ws_client.lock().await;
                        if let Some(client) = ws_client_guard.as_ref() {
                            client
                                .send(ClientMessage::DeleteDocument {
                                    document_id: pending_info.id,
                                })
                                .await?;
                        } else {
                            return Err(ClientError::WebSocket(
                                "Not connected during reconnection sync".to_string(),
                            ))?;
                        }
                    } else {
                        // Check if we have a queued patch to determine if this is create or update
                        // With server-authoritative versioning, we can't rely on version number anymore
                        match db.get_queued_patch(&pending_info.id).await {
                            Ok(Some((json_patch, old_hash_opt))) => {
                                // Have a queued patch = this is an UPDATE
                                tracing::info!(
                                    "CLIENT {}: Found queued patch for doc {} - using UpdateDocument",
                                    client_id,
                                    pending_info.id
                                );

                                // Convert the stored patch to DocumentPatch
                                use replicant_core::models::DocumentPatch;
                                use replicant_core::patches::calculate_checksum;

                                let content_hash = old_hash_opt
                                    .unwrap_or_else(|| calculate_checksum(&doc.content));

                                let patch_result = DocumentPatch {
                                    document_id: pending_info.id,
                                    patch: json_patch,
                                    content_hash,
                                };

                                // Track this upload
                                pending_uploads.lock().await.insert(
                                    pending_info.id,
                                    PendingUpload {
                                        operation_type: UploadType::Update,
                                        sent_at: Instant::now(),
                                    },
                                );

                                let ws_client_guard = ws_client.lock().await;
                                if let Some(client) = ws_client_guard.as_ref() {
                                    client
                                        .send(ClientMessage::UpdateDocument {
                                            patch: patch_result,
                                        })
                                        .await?;
                                } else {
                                    return Err(ClientError::WebSocket(
                                        "Not connected during reconnection sync".to_string(),
                                    ))?;
                                }
                            }
                            Ok(None) | Err(_) => {
                                // No queued patch = this is a CREATE
                                tracing::info!(
                                    "CLIENT {}: No queued patch for doc {} - using CreateDocument",
                                    client_id,
                                    pending_info.id
                                );

                                // Track this upload
                                pending_uploads.lock().await.insert(
                                    pending_info.id,
                                    PendingUpload {
                                        operation_type: UploadType::Create,
                                        sent_at: Instant::now(),
                                    },
                                );

                                let ws_client_guard = ws_client.lock().await;
                                if let Some(client) = ws_client_guard.as_ref() {
                                    client
                                        .send(ClientMessage::CreateDocument {
                                            document: doc.clone(),
                                        })
                                        .await?;
                                } else {
                                    return Err(ClientError::WebSocket(
                                        "Not connected during reconnection sync".to_string(),
                                    ))?;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "CLIENT {}: Failed to get pending document {}: {}",
                        client_id,
                        pending_info.id,
                        e
                    );
                }
            }
        }

        tracing::info!(
            "CLIENT {}: ✅ Completed uploading pending documents after reconnection",
            client_id
        );
        Ok(())
    }
}
