use crate::{
    database::{ClientDatabase, DocumentPreImage},
    error_code::ReplicantErrorCode,
    events::{EventDispatcher, EventOrigin},
    websocket::WebSocketClient,
};
use replicant_core::{
    errors::ClientError,
    models::{Document, SyncStatus},
    patches::{apply_patch, calculate_checksum, create_patch},
    protocol::{ClientMessage, ErrorCode, ServerMessage},
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

/// How long `shutdown` waits for the `ws_client` guard before giving up on
/// closing the socket cleanly. Short on purpose: it is a courtesy, not a step
/// anything else depends on (see `Client::shutdown`).
const SOCKET_RELEASE_TIMEOUT: Duration = Duration::from_millis(500);

/// Rebase rounds a single document may spend on `hash_mismatch` in one session.
///
/// Two clients writing to the same document in a tight loop can reject each
/// other indefinitely; the budget turns that live-lock into a `Conflict` the
/// user can see.
const MAX_REBASE_ATTEMPTS: u32 = 3;

/// Whether `shutdown` has begun. If so, also marks the client disconnected,
/// since a connect that finished meanwhile may have marked it connected.
fn shutdown_requested(stopped: &AtomicBool, is_connected: &AtomicBool) -> bool {
    if stopped.load(Ordering::SeqCst) {
        is_connected.store(false, Ordering::SeqCst);
        true
    } else {
        false
    }
}

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

/// What the next upload for a pending document has to be.
enum PendingUploadKind {
    Create,
    Update {
        patch: json_patch::Patch,
        old_content_hash: Option<String>,
    },
}

/// Decide whether a pending document uploads as a create or an update.
///
/// An unconsumed `create` row wins over anything else queued: the document has
/// never reached the server, so an update for it is answered `not_found` and the
/// document sits Pending forever. The create sends the document's CURRENT
/// content, which already folds in every local edit made since — and the ack
/// clears the whole queue for the document, so the update row those edits built
/// has nothing left to say.
///
/// Without a create row the queued update is the signal, exactly as before: a
/// cumulative patch means update, nothing queued means the document is new to
/// the server. A queue that cannot be read falls back to create, which is what
/// the reconnect and immediate-sync paths have always done.
async fn classify_pending_upload(db: &ClientDatabase, document_id: &Uuid) -> PendingUploadKind {
    match db.has_queued_create(document_id).await {
        Ok(true) => return PendingUploadKind::Create,
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(
                "CLIENT: Could not read the queued create for {} ({}), classifying by patch",
                document_id,
                e
            );
        }
    }

    match db.get_queued_patch(document_id).await {
        Ok(Some((patch, old_content_hash))) => PendingUploadKind::Update {
            patch,
            old_content_hash,
        },
        Ok(None) => PendingUploadKind::Create,
        Err(e) => {
            tracing::warn!(
                "CLIENT: Could not read the queued patch for {} ({}), sending a create",
                document_id,
                e
            );
            PendingUploadKind::Create
        }
    }
}

/// Where `resync_document` gets authoritative document state.
///
/// `Socket` is the production source. The socket handle is cloned out from
/// under a short-lived guard so the 30s call is never made while holding the
/// `ws_client` mutex, which reconnection and the heartbeat also need.
pub(crate) enum ResyncSource {
    Socket(Arc<Mutex<Option<WebSocketClient>>>),
    /// Test-only: records every resync attempt and returns a canned reply, so
    /// tests can assert that a resync was attempted rather than inferring it
    /// from the absence of a write.
    #[cfg(test)]
    Canned(Arc<CannedResync>),
}

#[cfg(test)]
pub(crate) struct CannedResync {
    pub attempts: Mutex<Vec<Uuid>>,
    /// Replies handed out one per fetch, in order. An exhausted queue behaves
    /// like an unavailable socket, which is how "offline" is modelled.
    pub replies: Mutex<std::collections::VecDeque<ServerMessage>>,
}

/// One-shot, per-document recovery for uploads the server rejected.
///
/// Settling a failed ack (removing the pending entry so later patches stop
/// deferring behind it) would otherwise cost the retry that the initial-sync
/// timeout used to provide. The document is still `Pending` with its sync_queue
/// row intact, so re-running the pending-document upload pass is enough.
///
/// Bounded to one trigger per document per session so a permanently rejected
/// document cannot spin the retry loop.
#[derive(Clone)]
pub(crate) struct UploadRetry {
    trigger: mpsc::Sender<()>,
    already_retried: Arc<Mutex<std::collections::HashSet<Uuid>>>,
    rebase_attempts: Arc<Mutex<HashMap<Uuid, u32>>>,
}

impl UploadRetry {
    fn new(trigger: mpsc::Sender<()>) -> Self {
        Self {
            trigger,
            already_retried: Arc::new(Mutex::new(std::collections::HashSet::new())),
            rebase_attempts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Claim the single recovery attempt a document gets per session.
    ///
    /// `false` means it has already been spent and the caller must not try
    /// again — the bound that keeps a repeating rejection from spinning.
    async fn claim_once(&self, client_id: Uuid, document_id: Uuid) -> bool {
        if !self.already_retried.lock().await.insert(document_id) {
            tracing::warn!(
                "CLIENT {}: Upload for {} failed again, not retrying a second time",
                client_id,
                document_id
            );
            return false;
        }
        true
    }

    async fn schedule(&self, client_id: Uuid, document_id: Uuid) {
        if !self.claim_once(client_id, document_id).await {
            return;
        }

        self.trigger_upload_pass(client_id, document_id).await;
    }

    /// Claim one of [`MAX_REBASE_ATTEMPTS`] rebase rounds for `document_id`.
    ///
    /// A rebase-resend is not the blind retry `schedule` bounds: every round
    /// rewrites the queued patch against fresher server state, so repeating it
    /// is progress rather than a loop. The budget exists only so two clients
    /// writing in lockstep cannot reject each other forever. `false` means the
    /// budget is spent and the caller must settle the document as a conflict.
    async fn claim_rebase(&self, client_id: Uuid, document_id: Uuid) -> bool {
        let mut attempts = self.rebase_attempts.lock().await;
        let used = attempts.entry(document_id).or_insert(0);
        if *used >= MAX_REBASE_ATTEMPTS {
            tracing::warn!(
                "CLIENT {}: Document {} has used all {} rebase attempts this session",
                client_id,
                document_id,
                MAX_REBASE_ATTEMPTS
            );
            return false;
        }
        *used += 1;
        true
    }

    /// Re-run the pending-upload pass so a freshly rebased patch is sent.
    ///
    /// Deliberately not once-per-session like `schedule`: the queued patch has
    /// changed, so a second send is legitimate. `claim_rebase` is the bound.
    async fn resend_after_rebase(&self, client_id: Uuid, document_id: Uuid) {
        self.trigger_upload_pass(client_id, document_id).await;
    }

    async fn trigger_upload_pass(&self, client_id: Uuid, document_id: Uuid) {
        match self.trigger.try_send(()) {
            Ok(()) => tracing::info!(
                "CLIENT {}: Scheduled an upload pass for {}",
                client_id,
                document_id
            ),
            Err(e) => tracing::warn!(
                "CLIENT {}: Could not schedule an upload pass for {}: {}",
                client_id,
                document_id,
                e
            ),
        }
    }
}

/// The status to write when server state is adopted wholesale.
///
/// An unresolved `Conflict` survives: the flag says the user still has to look
/// at this document, and taking the server's content does not answer that. Only
/// a genuine local edit clears it.
fn settled_status(current: SyncStatus) -> SyncStatus {
    match current {
        SyncStatus::Conflict => SyncStatus::Conflict,
        _ => SyncStatus::Synced,
    }
}

/// The server state carried by a `hash_mismatch` rejection.
///
/// The server sends what it currently holds so the client can rebase onto it;
/// every other rejection reason arrives without these fields.
struct HashMismatch {
    current_revision: i64,
    current_content: serde_json::Value,
    current_hash: String,
}

impl ResyncSource {
    fn socket(ws_client: &Arc<Mutex<Option<WebSocketClient>>>) -> Self {
        Self::Socket(ws_client.clone())
    }

    /// `None` means "no source available" (offline), which callers treat as
    /// leave-it-unsynced rather than as an error.
    async fn fetch(&self, document_id: Uuid) -> Option<ServerMessage> {
        match self {
            Self::Socket(ws_client) => {
                let ws = ws_client.lock().await.clone()?;
                Some(ws.fetch_document(document_id).await)
            }
            #[cfg(test)]
            Self::Canned(canned) => {
                canned.attempts.lock().await.push(document_id);
                canned.replies.lock().await.pop_front()
            }
        }
    }
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
    // One-shot recovery for uploads the server rejects
    upload_retry: UploadRetry,
    // Sync is possible for this instance (credentials + adopted identity).
    // Immutable for the client's lifetime: enrollment recreates the client.
    sync_enabled: bool,
    /// Set by `shutdown`; the reconnection monitor stops when it sees it.
    stopped: Arc<AtomicBool>,
}

impl Client {
    pub async fn new(
        database_url: &str,
        server_url: &str,
        email: &str,
        api_key: &str,
        api_secret: &str,
        canonical_user_id: Option<Uuid>,
    ) -> SyncResult<Self> {
        Self::with_event_dispatcher(
            database_url,
            server_url,
            email,
            api_key,
            api_secret,
            canonical_user_id,
            None,
        )
        .await
    }

    /// Opens the database, connects if sync is enabled, then announces the
    /// connection and runs the initial sync (see [`Client::start`]).
    pub async fn with_event_dispatcher(
        database_url: &str,
        server_url: &str,
        email: &str,
        api_key: &str,
        api_secret: &str,
        canonical_user_id: Option<Uuid>,
        event_dispatcher: Option<Arc<EventDispatcher>>,
    ) -> SyncResult<Self> {
        let client = Self::open(
            database_url,
            server_url,
            email,
            api_key,
            api_secret,
            canonical_user_id,
            event_dispatcher,
        )
        .await?;
        client.start().await;
        Ok(client)
    }

    /// Opens the database and connects if sync is enabled, without emitting
    /// ConnectionSucceeded, monitoring the connection or syncing. The caller
    /// must call [`Client::start`] once the client is ready for use.
    pub(crate) async fn open(
        database_url: &str,
        server_url: &str,
        email: &str,
        api_key: &str,
        api_secret: &str,
        canonical_user_id: Option<Uuid>,
        event_dispatcher: Option<Arc<EventDispatcher>>,
    ) -> SyncResult<Self> {
        let db = Arc::new(ClientDatabase::new(database_url).await?);

        // Everything after the pool exists runs in `build`, so that a failure
        // anywhere in it still closes the pool. Dropping a `ClientDatabase`
        // un-closed leaves one detached sqlx worker thread per connection
        // running this module's code for the life of the process — only
        // `Pool::close()` joins them (DEV-1118). Failing here is not exotic: it
        // is what a contended migration does once it outlasts sqlite's busy
        // timeout, which is the very scenario that produced the crash.
        match Self::build(
            db.clone(),
            server_url,
            email,
            api_key,
            api_secret,
            canonical_user_id,
            event_dispatcher,
        )
        .await
        {
            Ok(client) => Ok(client),
            Err(e) => {
                db.close().await;
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn build(
        db: Arc<ClientDatabase>,
        server_url: &str,
        email: &str,
        api_key: &str,
        api_secret: &str,
        canonical_user_id: Option<Uuid>,
        event_dispatcher: Option<Arc<EventDispatcher>>,
    ) -> SyncResult<Self> {
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
        let reconnect_sync_tx_for_retry = reconnect_sync_tx.clone();

        let is_connected = Arc::new(AtomicBool::new(false));

        // Adopt the canonical identity (from stored credentials) BEFORE any
        // WebSocket work: no live sync exists yet and the handle hasn't been
        // returned, so adoption cannot race document creation.
        let identity_adopted = db.is_identity_adopted().await.unwrap_or(false);
        match (canonical_user_id, identity_adopted) {
            (Some(canonical), false) => {
                db.adopt_identity(user_id, canonical).await?;
                event_dispatcher.emit_identity_changed(&user_id, &canonical, email);
            }
            (Some(canonical), true) if canonical != user_id => {
                return Err(replicant_core::errors::SyncError::InvalidOperation(
                    format!(
                        "account switch not supported: credentials belong to user {} but this \
                     database is owned by user {}; reset local data to enroll a different account",
                        canonical, user_id
                    ),
                ));
            }
            _ => {}
        }
        let user_id = db.get_user_id().await?;

        // Sync requires credentials and an adopted (server-confirmed)
        // identity; otherwise stay local-only and never join a sync topic.
        let sync_enabled = !api_key.is_empty() && db.is_identity_adopted().await.unwrap_or(false);

        let (ws_client, initial_ping_time) = if sync_enabled {
            // Try to connect to WebSocket, but don't fail if offline
            match WebSocketClient::connect(
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
            }
        } else {
            (None, None)
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
            upload_retry: UploadRetry::new(reconnect_sync_tx_for_retry),
            sync_enabled,
            stopped: Arc::new(AtomicBool::new(false)),
        };

        // Automatically start background tasks
        engine.spawn_background_tasks().await?;

        Ok(engine)
    }

    pub fn event_dispatcher(&self) -> Arc<EventDispatcher> {
        self.event_dispatcher.clone()
    }

    /// Releases everything this client holds that outlives a plain drop: the
    /// WebSocket and, most importantly, the sqlite pool.
    ///
    /// Dropping the client only drops its `Pool` handle, which marks the pool
    /// closed without waiting for anything; each sqlite connection lives on a
    /// dedicated OS thread that sqlx spawns and never joins, so a drop can
    /// leave those threads running. Awaiting `close()` waits for them to
    /// finish, which matters when the caller is a dynamically loaded module
    /// about to be unmapped (DEV-1118).
    ///
    /// Idempotent, and safe to call while background tasks are still running:
    /// they see a closed pool and error out rather than hang.
    ///
    /// The pool is closed FIRST and unconditionally. Taking the socket first
    /// looks tidier — background work would stop querying a pool that is about
    /// to close — but `ws_client` is an async mutex that live tasks hold across
    /// un-timed network sends (`send`, the heartbeat, `fetch_document`), so on a
    /// half-open socket that guard may never arrive. Making it a precondition
    /// for closing the pool would mean a stalled socket keeps every sqlx worker
    /// thread alive for the life of the process, which is the very failure this
    /// shutdown exists to prevent.
    pub async fn shutdown(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.is_connected.store(false, Ordering::SeqCst);
        self.db.close().await;

        // Best effort, and bounded: nothing downstream depends on it, since a
        // closed pool already stops the background tasks, and the socket goes
        // down with the runtime moments later either way.
        match tokio::time::timeout(SOCKET_RELEASE_TIMEOUT, self.ws_client.lock()).await {
            Ok(mut guard) => {
                guard.take();
            }
            Err(_) => {
                tracing::warn!(
                    "CLIENT {}: socket still in use after {:?} during shutdown; \
                     leaving it to the runtime",
                    self.client_id,
                    SOCKET_RELEASE_TIMEOUT
                );
            }
        }
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
        let ws_client_for_handler = ws_client.clone();
        let deferred_messages = self.deferred_messages.clone();
        let upload_retry = self.upload_retry.clone();

        // Clone variables for the reconnection sync handler
        let db_for_reconnect_sync = db.clone();
        let pending_uploads_for_reconnect_sync = pending_uploads.clone();
        let ws_client_for_reconnect_sync = ws_client.clone();
        let event_dispatcher_for_reconnect_sync = event_dispatcher.clone();

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
                    &upload_retry,
                    &ResyncSource::socket(&ws_client_for_handler),
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
                        event_dispatcher_for_reconnect_sync.emit_sync_started();
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

        Ok(())
    }

    /// Emits ConnectionSucceeded if the start-up connect succeeded, starts the
    /// reconnection monitor, then runs the initial upload-first sync.
    ///
    /// A failure is reported as SyncError and leaves a working client: upload
    /// protection is off, and the monitor and later upload passes carry on.
    pub(crate) async fn start(&self) {
        let connected = self.is_connected();
        if connected {
            self.event_dispatcher
                .emit_connection_succeeded(&self.server_url);
        }
        self.start_reconnection_loop();

        if !connected {
            tracing::info!(
                "CLIENT {}: Starting in offline mode - will sync when connection available",
                self.client_id
            );
            return;
        }

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
        if let Err(e) = self.sync_pending_documents().await {
            self.end_upload_protection().await;
            self.event_dispatcher.emit_sync_error(
                ReplicantErrorCode::Unknown,
                &format!("Initial upload failed: {}", e),
            );
            return;
        }

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
                    tracing::info!("CLIENT {}: All uploads settled", self.client_id);
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

        self.end_upload_protection().await;

        // Second: Download current server state (which now includes our uploaded documents)
        tracing::info!(
            "CLIENT {}: Upload phase complete, requesting server state",
            self.client_id
        );
        if let Err(e) = self.sync_all().await {
            self.event_dispatcher.emit_sync_error(
                ReplicantErrorCode::ConnectionFailed,
                &format!("Full sync failed: {}", e),
            );
        }
    }

    /// Ends upload protection and applies the server messages it deferred.
    async fn end_upload_protection(&self) {
        self.sync_protection_mode.store(false, Ordering::Relaxed);
        tracing::info!(
            "CLIENT {}: Protection mode DISABLED - server sync now allowed",
            self.client_id
        );

        if let Err(e) = Self::process_deferred_messages(
            &self.deferred_messages,
            &self.db,
            self.client_id,
            &self.event_dispatcher,
            &self.pending_uploads,
            &ResyncSource::socket(&self.ws_client),
        )
        .await
        {
            tracing::error!(
                "CLIENT {}: Error processing deferred messages: {}",
                self.client_id,
                e
            );
        }
    }

    pub async fn create_document(&self, content: serde_json::Value) -> SyncResult<Document> {
        self.create_document_with_id(Uuid::new_v4(), content).await
    }

    /// Server-authoritative user id, fixed for the client's lifetime.
    /// Identity adoption runs in `Client::new` before this is read;
    /// enrolling afterwards recreates the client.
    pub fn user_id(&self) -> Uuid {
        self.user_id
    }

    pub async fn create_document_with_id(
        &self,
        id: Uuid,
        content: serde_json::Value,
    ) -> SyncResult<Document> {
        let doc = Document {
            id,
            user_id: Some(self.user_id()),
            content,
            sync_revision: 1,
            content_hash: None,
            title: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            deleted_at: None,
            author_name: None,
            visibility: None,
            provenance: None,
        };

        tracing::info!(
            "CLIENT {}: Creating document locally: {}",
            self.client_id,
            doc.id
        );
        // Queue the create alongside the save: until the server acks it, that
        // row is what tells the upload path this document must be created and
        // not patched, however many local edits land in the meantime.
        self.db.save_new_document_and_queue_create(&doc).await?;

        self.event_dispatcher
            .emit_document_created_with_attribution(
                &doc.id,
                &doc.content,
                doc.user_id.as_ref(),
                doc.author_name.as_deref(),
                doc.visibility.as_deref(),
                EventOrigin::Local,
            );

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
        use replicant_core::protocol::ChangeEventType;

        tracing::info!(
            "CLIENT {}: 📋 Atomically saving document and queueing patch for doc {}",
            self.client_id,
            doc.id
        );
        // The queue keeps the base of the FIRST unsent edit, so a run of offline
        // edits uploads as one diff against the state the server holds.
        self.db
            .save_document_and_queue_patch(
                &doc,
                &patch,
                ChangeEventType::Update,
                Some(&old_content),
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
            .emit_document_updated_with_attribution(
                &doc.id,
                &doc.content,
                doc.user_id.as_ref(),
                doc.author_name.as_deref(),
                doc.visibility.as_deref(),
                EventOrigin::Local,
            );

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
        self.event_dispatcher
            .emit_document_deleted(&id, EventOrigin::Local);

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

    pub async fn get_all_document_ids(&self, include_deleted: bool) -> SyncResult<Vec<Uuid>> {
        let ids = self.db.get_all_document_ids(include_deleted).await?;
        tracing::info!(
            "CLIENT: get_all_document_ids({}) returning {} ids",
            include_deleted,
            ids.len()
        );
        Ok(ids)
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
                            if let Err(e) = client
                                .send(ClientMessage::DeleteDocument {
                                    document_id: pending_info.id,
                                })
                                .await
                            {
                                self.pending_uploads.lock().await.remove(&pending_info.id);
                                return Err(e);
                            }
                        } else {
                            self.pending_uploads.lock().await.remove(&pending_info.id);
                            return Err(ClientError::WebSocket("Not connected".to_string()))?;
                        }

                        UploadType::Delete
                    } else {
                        // A queued create outranks a queued patch; see
                        // `classify_pending_upload`.
                        match classify_pending_upload(&self.db, &pending_info.id).await {
                            PendingUploadKind::Update {
                                patch: stored_patch,
                                old_content_hash: old_hash_opt,
                            } => {
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
                                    if let Err(e) = client
                                        .send(ClientMessage::UpdateDocument {
                                            patch: document_patch,
                                        })
                                        .await
                                    {
                                        self.pending_uploads.lock().await.remove(&pending_info.id);
                                        return Err(e);
                                    }
                                } else {
                                    self.pending_uploads.lock().await.remove(&pending_info.id);
                                    return Err(ClientError::WebSocket(
                                        "Not connected".to_string(),
                                    ))?;
                                }

                                UploadType::Update
                            }
                            PendingUploadKind::Create => {
                                tracing::info!(
                                    "CLIENT {}: Doc {} has not reached the server - treating as CREATE",
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
                                    if let Err(e) = client
                                        .send(ClientMessage::CreateDocument {
                                            document: doc.clone(),
                                        })
                                        .await
                                    {
                                        self.pending_uploads.lock().await.remove(&pending_info.id);
                                        return Err(e);
                                    }
                                } else {
                                    self.pending_uploads.lock().await.remove(&pending_info.id);
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
        upload_retry: &UploadRetry,
        source: &ResyncSource,
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
                // The upload is over either way. A failed ack must clear the
                // pending entry and drain the queue exactly like a successful
                // one: otherwise every later patch for this document defers
                // forever behind an upload that will never complete.
                // A hash_mismatch is not a plain rejection: the server told us
                // what it currently holds, so the queued patch can be rebased
                // onto it and resent. That path triggers its own upload pass,
                // so the blind retry must not also fire.
                let rebase = if *success {
                    None
                } else {
                    Self::hash_mismatch_details(&msg)
                };

                // A rejected create cannot be fixed by resending it: the server
                // either already holds the document (lost ack, or the
                // content-hash dedupe) or refuses it outright, and both survive
                // a blind retry. It gets its own recovery below instead.
                let document_id = *document_id;
                let failed_create = !*success
                    && rebase.is_none()
                    && matches!(msg, ServerMessage::DocumentCreatedResponse { .. });

                if !*success {
                    tracing::error!(
                        "CLIENT {}: Upload failed for document {}",
                        client_id,
                        document_id
                    );
                    if rebase.is_none() && !failed_create {
                        // The document is still Pending with its queue row
                        // intact, so a retry pass will pick it up.
                        upload_retry.schedule(client_id, document_id).await;
                    }
                }

                let mut uploads = pending_uploads.lock().await;
                if let Some(upload) = uploads.remove(&document_id) {
                    let elapsed = upload.sent_at.elapsed();
                    tracing::info!(
                        "CLIENT {}: Upload settled for {} ({:?}, success={}) in {:?}",
                        client_id,
                        document_id,
                        upload.operation_type,
                        success,
                        elapsed
                    );

                    // If this was the last pending upload, notify
                    if uploads.is_empty() {
                        tracing::info!(
                            "CLIENT {}: All uploads settled - notifying completion",
                            client_id
                        );
                        upload_complete_notifier.notify_one();
                    }
                }
                // Release the lock before processing deferred messages
                drop(uploads);

                // Replay anything queued while this document was uploading.
                if let Err(e) = Self::process_deferred_messages(
                    deferred_messages,
                    db,
                    client_id,
                    event_dispatcher,
                    pending_uploads,
                    source,
                )
                .await
                {
                    tracing::error!(
                        "CLIENT {}: Error processing deferred messages after upload: {}",
                        client_id,
                        e
                    );
                }

                // The pending entry is cleared and the queue drained, so a
                // resend can re-register cleanly.
                if let Some(server) = rebase {
                    return Self::rebase_after_hash_mismatch(
                        db,
                        client_id,
                        event_dispatcher,
                        upload_retry,
                        document_id,
                        server,
                    )
                    .await;
                }

                // Normal processing first, so the plain failure event still
                // reaches the consumer before any recovery event.
                Self::handle_server_message(msg, db, client_id, event_dispatcher, source).await?;

                if failed_create {
                    return Self::recover_failed_create(
                        db,
                        source,
                        client_id,
                        event_dispatcher,
                        upload_retry,
                        document_id,
                    )
                    .await;
                }

                return Ok(());
            }

            // A patch for a document we are still uploading is almost always
            // our own broadcast echoed back (DEV-1038). Applying the guard now
            // would see status=Pending and burn a resync round-trip on every
            // local edit; deferring lets the ack land first, after which the
            // echo hits the idempotency drop for free.
            ServerMessage::DocumentUpdated { patch } => {
                if Self::has_pending_upload(pending_uploads, &patch.document_id).await {
                    tracing::info!(
                        "CLIENT {}: Deferring patch for {} (upload in progress)",
                        client_id,
                        patch.document_id
                    );
                    Self::defer_message(
                        deferred_messages,
                        client_id,
                        ServerMessage::DocumentUpdated {
                            patch: patch.clone(),
                        },
                    )
                    .await;
                    return Ok(());
                }

                return Self::handle_server_message(msg, db, client_id, event_dispatcher, source)
                    .await;
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
                    Self::defer_message(
                        deferred_messages,
                        client_id,
                        ServerMessage::SyncDocument {
                            document: document.clone(),
                        },
                    )
                    .await;
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
                        Self::defer_message(
                            deferred_messages,
                            client_id,
                            ServerMessage::SyncDocument {
                                document: document.clone(),
                            },
                        )
                        .await;
                        return Ok(());
                    }
                }

                // Safe to proceed with sync
                return Self::handle_server_message(msg, db, client_id, event_dispatcher, source)
                    .await;
            }

            _ => {
                // For all other messages, use normal handling
                return Self::handle_server_message(msg, db, client_id, event_dispatcher, source)
                    .await;
            }
        }
    }

    /// Process all deferred sync messages that were queued during upload protection
    async fn process_deferred_messages(
        deferred_messages: &Arc<Mutex<Vec<ServerMessage>>>,
        db: &Arc<ClientDatabase>,
        client_id: Uuid,
        event_dispatcher: &Arc<EventDispatcher>,
        pending_uploads: &Arc<Mutex<HashMap<Uuid, PendingUpload>>>,
        source: &ResyncSource,
    ) -> SyncResult<()> {
        // Drain into a local batch and release the lock before handling: the
        // handlers below may defer again, which needs the same lock.
        let batch: Vec<ServerMessage> = {
            let mut messages = deferred_messages.lock().await;
            if messages.is_empty() {
                return Ok(());
            }
            messages.drain(..).collect()
        };

        tracing::info!(
            "CLIENT {}: Processing {} deferred sync messages",
            client_id,
            batch.len()
        );

        for msg in batch {
            // One ack drains the whole queue, but other documents may still be
            // uploading. Replaying their messages now would resync against
            // state we are about to overwrite, so re-defer them instead.
            if let Some((_, document_id)) = Self::deferral_key(&msg) {
                if Self::has_pending_upload(pending_uploads, &document_id).await {
                    tracing::info!(
                        "CLIENT {}: Re-deferring message for {} (upload still in progress)",
                        client_id,
                        document_id
                    );
                    Self::redefer_message(deferred_messages, client_id, msg).await;
                    continue;
                }
            }

            if let Err(e) =
                Self::handle_server_message(msg, db, client_id, event_dispatcher, source).await
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

    /// Identity of a deferrable message: its variant plus the document it
    /// concerns. Two messages with the same key are interchangeable — both
    /// carry "the server's latest state for this document" — so the newer one
    /// can replace the older.
    fn deferral_key(msg: &ServerMessage) -> Option<(&'static str, Uuid)> {
        match msg {
            ServerMessage::DocumentUpdated { patch } => {
                Some(("document_updated", patch.document_id))
            }
            ServerMessage::SyncDocument { document } => Some(("sync_document", document.id)),
            _ => None,
        }
    }

    /// The server revision a deferrable message carries, used to decide which
    /// of two entries for the same document is newer.
    fn deferral_revision(msg: &ServerMessage) -> i64 {
        match msg {
            ServerMessage::DocumentUpdated { patch } => patch.sync_revision,
            ServerMessage::SyncDocument { document } => document.sync_revision,
            _ => 0,
        }
    }

    /// Queue a message for replay once the in-flight upload completes.
    ///
    /// Entries are collapsed per (variant, document), keeping whichever carries
    /// the higher revision. Collapsing is what keeps a burst of echoes for one
    /// document from evicting a `SyncDocument` for another — and a dropped
    /// `SyncDocument` never self-heals, it just leaves that document silently
    /// stale.
    ///
    /// Dropping intermediate revisions is safe but not free: the survivor will
    /// usually fail the contiguity check on replay and cost one resync
    /// round-trip, which is still cheaper than replaying every echo.
    ///
    /// Producers run on several tasks (initial-sync drain, handler, and the
    /// post-reconnect handler), and the drain no longer holds the lock, so the
    /// revision comparison — not arrival order — is what decides the winner.
    async fn defer_message(
        deferred_messages: &Arc<Mutex<Vec<ServerMessage>>>,
        client_id: Uuid,
        msg: ServerMessage,
    ) {
        Self::defer_message_inner(deferred_messages, client_id, msg, false).await
    }

    /// Re-queue a message pulled off the queue that turned out not to be
    /// replayable yet. A queued entry is never older than what we are putting
    /// back, so this must not overwrite a newer entry that landed meanwhile.
    async fn redefer_message(
        deferred_messages: &Arc<Mutex<Vec<ServerMessage>>>,
        client_id: Uuid,
        msg: ServerMessage,
    ) {
        Self::defer_message_inner(deferred_messages, client_id, msg, true).await
    }

    async fn defer_message_inner(
        deferred_messages: &Arc<Mutex<Vec<ServerMessage>>>,
        client_id: Uuid,
        msg: ServerMessage,
        insert_if_absent_only: bool,
    ) {
        const MAX_DEFERRED_MESSAGES: usize = 100;
        let mut queue = deferred_messages.lock().await;

        if let Some(key) = Self::deferral_key(&msg) {
            if let Some(slot) = queue
                .iter_mut()
                .find(|queued| Self::deferral_key(queued) == Some(key))
            {
                if insert_if_absent_only {
                    return;
                }
                if Self::deferral_revision(&msg) > Self::deferral_revision(slot) {
                    *slot = msg;
                }
                return;
            }
        }

        if queue.len() >= MAX_DEFERRED_MESSAGES {
            tracing::warn!(
                "CLIENT {}: Deferred queue full ({} messages), dropping oldest",
                client_id,
                queue.len()
            );
            queue.remove(0);
        }
        queue.push(msg);
    }

    // Check if a document has an active upload in progress
    async fn has_pending_upload(
        pending_uploads: &Arc<Mutex<HashMap<Uuid, PendingUpload>>>,
        document_id: &Uuid,
    ) -> bool {
        let uploads = pending_uploads.lock().await;
        uploads.contains_key(document_id)
    }

    /// The server state a `hash_mismatch` rejection carries, or `None` for any
    /// other ack. All three fields are required: a partial reply cannot be
    /// rebased onto and falls back to the blind retry.
    fn hash_mismatch_details(msg: &ServerMessage) -> Option<HashMismatch> {
        let ServerMessage::DocumentUpdatedResponse {
            reason,
            current_revision,
            current_content,
            current_hash,
            ..
        } = msg
        else {
            return None;
        };

        if reason.as_deref() != Some("hash_mismatch") {
            return None;
        }

        Some(HashMismatch {
            current_revision: (*current_revision)?,
            current_content: current_content.clone()?,
            current_hash: current_hash.clone()?,
        })
    }

    /// Turn a `hash_mismatch` rejection into a rebase-and-resend.
    ///
    /// The queued patch is replayed onto the server's current content. If it
    /// applies, the merged result becomes local truth at the server's revision,
    /// a fresh patch is queued against `current_hash`, and the upload pass runs
    /// again. Because the replay starts from the server's content, an edit to a
    /// different field survives the round trip instead of being overwritten.
    ///
    /// Anything that stops the rebase — a patch that no longer applies, a
    /// missing queue row, an exhausted attempt budget — settles the document as
    /// `Conflict` with the server's copy as local truth.
    async fn rebase_after_hash_mismatch(
        db: &Arc<ClientDatabase>,
        client_id: Uuid,
        event_dispatcher: &Arc<EventDispatcher>,
        upload_retry: &UploadRetry,
        document_id: Uuid,
        server: HashMismatch,
    ) -> SyncResult<()> {
        let Some(status) = db.get_sync_status(&document_id).await? else {
            tracing::warn!(
                "CLIENT {}: Rejected update for {} has no local row, nothing to rebase",
                client_id,
                document_id
            );
            return Ok(());
        };

        let local = match db.get_document(&document_id).await {
            Ok(doc) => doc,
            Err(e) => {
                // Writing on a failed read would overwrite state we never saw.
                tracing::warn!(
                    "CLIENT {}: Could not read {} to rebase it ({}), leaving it untouched",
                    client_id,
                    document_id,
                    e
                );
                return Ok(());
            }
        };

        let expected = DocumentPreImage {
            sync_revision: local.sync_revision,
            content: local.content.clone(),
            sync_status: status,
        };

        // Read the patch before spending a budget unit: a document with nothing
        // queued is unresolvable, not live-locked.
        let Ok(Some((queued, _))) = db.get_queued_patch(&document_id).await else {
            return Self::settle_as_conflict(
                db,
                client_id,
                event_dispatcher,
                document_id,
                &server,
                &expected,
                "there is no queued patch to rebase",
            )
            .await;
        };

        if !upload_retry.claim_rebase(client_id, document_id).await {
            return Self::settle_as_conflict(
                db,
                client_id,
                event_dispatcher,
                document_id,
                &server,
                &expected,
                "the rebase attempt budget is exhausted",
            )
            .await;
        }

        let mut rebased = server.current_content.clone();
        if let Err(e) = apply_patch(&mut rebased, &queued) {
            return Self::settle_as_conflict(
                db,
                client_id,
                event_dispatcher,
                document_id,
                &server,
                &expected,
                &format!("the queued patch no longer applies ({})", e),
            )
            .await;
        }

        // Diff from the server's content rather than resending the original
        // patch: the server will apply this to exactly `current_content`.
        let forward = create_patch(&server.current_content, &rebased)?;

        if forward.0.is_empty() {
            // The winner's update already contains this edit — replaying it
            // would upload a no-op and take a full round trip to learn that.
            return Self::adopt_without_resending(
                db,
                client_id,
                event_dispatcher,
                document_id,
                upload_retry,
                &server,
                &expected,
                local,
            )
            .await;
        }

        if !db
            .rebase_pending_document_if_unchanged(
                &document_id,
                &rebased,
                server.current_revision,
                &server.current_content,
                &server.current_hash,
                SyncStatus::Pending,
                &expected,
            )
            .await?
        {
            // A newer local edit landed mid-rebase. It is queued against the
            // old base, so resending it earns another hash_mismatch and the
            // next round rebases from the newer state. The attempt budget
            // guarantees this terminates.
            tracing::warn!(
                "CLIENT {}: Rebase of {} raced a local edit, resending for another round",
                client_id,
                document_id
            );
            upload_retry
                .resend_after_rebase(client_id, document_id)
                .await;
            return Ok(());
        }

        tracing::info!(
            "CLIENT {}: Rebased {} onto server revision {} and requeued the edit",
            client_id,
            document_id,
            server.current_revision
        );

        event_dispatcher.emit_document_updated_with_attribution(
            &document_id,
            &rebased,
            local.user_id.as_ref(),
            local.author_name.as_deref(),
            local.visibility.as_deref(),
            EventOrigin::Remote,
        );

        upload_retry
            .resend_after_rebase(client_id, document_id)
            .await;
        Ok(())
    }

    /// The rebase produced exactly the server's content: the edit is already
    /// there. Take the server's revision, drop the queue row, and skip the
    /// upload entirely rather than paying a round trip for a no-op patch.
    #[allow(clippy::too_many_arguments)]
    async fn adopt_without_resending(
        db: &Arc<ClientDatabase>,
        client_id: Uuid,
        event_dispatcher: &Arc<EventDispatcher>,
        document_id: Uuid,
        upload_retry: &UploadRetry,
        server: &HashMismatch,
        expected: &DocumentPreImage,
        mut doc: Document,
    ) -> SyncResult<()> {
        doc.content = server.current_content.clone();
        doc.sync_revision = server.current_revision;
        doc.updated_at = chrono::Utc::now();

        // One transaction: a local edit landing between the write and the
        // delete would otherwise have its fresh queue row deleted, leaving a
        // Pending document with nothing queued.
        if !db
            .save_document_and_clear_queue_if_unchanged(
                &doc,
                settled_status(expected.sync_status),
                expected,
            )
            .await?
        {
            tracing::warn!(
                "CLIENT {}: Adopting {} raced a local edit, resending instead",
                client_id,
                document_id
            );
            upload_retry
                .resend_after_rebase(client_id, document_id)
                .await;
            return Ok(());
        }

        tracing::info!(
            "CLIENT {}: Rejected update for {} was already in the server's revision {}, nothing to resend",
            client_id,
            document_id,
            server.current_revision
        );

        event_dispatcher.emit_document_updated_with_attribution(
            &doc.id,
            &doc.content,
            doc.user_id.as_ref(),
            doc.author_name.as_deref(),
            doc.visibility.as_deref(),
            EventOrigin::Remote,
        );
        Ok(())
    }

    /// Give up on reconciling a rejected edit: the server's copy becomes local
    /// truth, the document is marked `Conflict`, and the host app is told.
    ///
    /// The queued patch is dropped — it is built on a base that no longer
    /// exists, so leaving it would resend and be rejected forever.
    #[allow(clippy::too_many_arguments)]
    async fn settle_as_conflict(
        db: &Arc<ClientDatabase>,
        client_id: Uuid,
        event_dispatcher: &Arc<EventDispatcher>,
        document_id: Uuid,
        server: &HashMismatch,
        expected: &DocumentPreImage,
        why: &str,
    ) -> SyncResult<()> {
        tracing::error!(
            "CLIENT {}: Cannot rebase {}: {}. Keeping the server's copy.",
            client_id,
            document_id,
            why
        );

        let mut doc = match db.get_document(&document_id).await {
            Ok(doc) => doc,
            Err(e) => {
                tracing::warn!(
                    "CLIENT {}: Could not read {} to settle the conflict ({})",
                    client_id,
                    document_id,
                    e
                );
                return Ok(());
            }
        };
        doc.content = server.current_content.clone();
        doc.sync_revision = server.current_revision;
        doc.updated_at = chrono::Utc::now();

        // One transaction, for the same reason as `adopt_without_resending`:
        // dropping the queue row must not outlive a failed swap, and must not
        // take a queue row a concurrent local edit just inserted.
        if !db
            .save_document_and_clear_queue_if_unchanged(&doc, SyncStatus::Conflict, expected)
            .await?
        {
            // A newer local edit landed; it will be uploaded and get its own
            // rejection, so do not clobber it here.
            tracing::warn!(
                "CLIENT {}: Conflict settlement for {} raced a local edit, leaving it pending",
                client_id,
                document_id
            );
            return Ok(());
        }

        event_dispatcher.emit_conflict_detected(&document_id);
        event_dispatcher.emit_sync_error(
            ReplicantErrorCode::UpdateConflict,
            &format!(
                "Update for {} could not be rebased ({}); the server's copy is now local truth",
                document_id, why
            ),
        );
        event_dispatcher.emit_document_updated_with_attribution(
            &doc.id,
            &doc.content,
            doc.user_id.as_ref(),
            doc.author_name.as_deref(),
            doc.visibility.as_deref(),
            EventOrigin::Remote,
        );

        Ok(())
    }

    /// Recover a document whose create the server rejected.
    ///
    /// A create fails for two very different reasons. Either the server already
    /// holds the document — a lost ack, or its content-hash dedupe returning an
    /// existing row — in which case the local copy must adopt server state and
    /// upload its edits as an update. Or the create was genuinely refused
    /// (validation, quota), which no amount of resending fixes.
    ///
    /// Fetching the document settles which it is. Adopting server state also
    /// consumes the queued `create`, so the retry sends an update rather than a
    /// second doomed create.
    ///
    /// One attempt per document per session, so a repeating refusal cannot spin.
    async fn recover_failed_create(
        db: &Arc<ClientDatabase>,
        source: &ResyncSource,
        client_id: Uuid,
        event_dispatcher: &Arc<EventDispatcher>,
        upload_retry: &UploadRetry,
        document_id: Uuid,
    ) -> SyncResult<()> {
        if !upload_retry.claim_once(client_id, document_id).await {
            return Ok(());
        }

        let Some(result) = source.fetch(document_id).await else {
            tracing::warn!(
                "CLIENT {}: Cannot check {} after a rejected create while offline, leaving it pending",
                client_id,
                document_id
            );
            return Ok(());
        };

        if let ServerMessage::Error {
            code: ErrorCode::DocumentNotFound,
            ..
        } = result
        {
            return Self::settle_rejected_create(db, client_id, event_dispatcher, document_id)
                .await;
        }

        tracing::info!(
            "CLIENT {}: Server already holds {}, adopting its state and resending as an update",
            client_id,
            document_id
        );
        Self::apply_resync_result(db, client_id, event_dispatcher, document_id, result).await?;
        upload_retry
            .resend_after_rebase(client_id, document_id)
            .await;
        Ok(())
    }

    /// Park a document the server refused to create and does not hold.
    ///
    /// `Conflict` takes it out of the pending set, so the upload pass stops
    /// resending a create that is only going to be refused again. Deliberately
    /// NOT soft-deleted the way a resync treats a missing document: this content
    /// has never reached the server, so the local copy is the only one there is.
    ///
    /// The queued rows stay. The document genuinely still owes the server a
    /// create, and a later session — after whatever refused it is resolved — can
    /// send one once a local edit makes it pending again.
    async fn settle_rejected_create(
        db: &Arc<ClientDatabase>,
        client_id: Uuid,
        event_dispatcher: &Arc<EventDispatcher>,
        document_id: Uuid,
    ) -> SyncResult<()> {
        tracing::error!(
            "CLIENT {}: Server refused to create {} and does not hold it, parking it as a conflict",
            client_id,
            document_id
        );

        let doc = match db.get_document(&document_id).await {
            Ok(doc) => doc,
            Err(e) => {
                tracing::warn!(
                    "CLIENT {}: Could not read {} to park the rejected create ({})",
                    client_id,
                    document_id,
                    e
                );
                return Ok(());
            }
        };
        db.save_document_with_status(&doc, Some(SyncStatus::Conflict))
            .await?;

        event_dispatcher.emit_conflict_detected(&document_id);
        event_dispatcher.emit_sync_error(
            ReplicantErrorCode::UpdateConflict,
            &format!(
                "The server refused to create {} and does not hold it; the local copy is unsynced",
                document_id
            ),
        );
        Ok(())
    }

    /// Fetch the authoritative document from the server and reconcile it with
    /// local state. Used whenever a broadcast patch cannot be trusted to apply
    /// (unknown doc, revision gap, unsent local edits, diverged result).
    ///
    /// Offline, this is a no-op: the document simply stays unsynced until a
    /// later resync.
    async fn resync_document(
        db: &Arc<ClientDatabase>,
        source: &ResyncSource,
        client_id: Uuid,
        event_dispatcher: &Arc<EventDispatcher>,
        document_id: Uuid,
    ) -> SyncResult<()> {
        // A lost compare-and-swap means a local edit landed mid-resync. Re-fetch
        // once and reconcile against the newer local state rather than leaving a
        // stale queued patch stranded until some unrelated broadcast arrives.
        const RESYNC_ATTEMPTS: usize = 2;

        for attempt in 1..=RESYNC_ATTEMPTS {
            let Some(result) = source.fetch(document_id).await else {
                tracing::warn!(
                    "CLIENT {}: Cannot resync {} while offline, leaving it unsynced",
                    client_id,
                    document_id
                );
                return Ok(());
            };

            if Self::apply_resync_result(db, client_id, event_dispatcher, document_id, result)
                .await?
            {
                return Ok(());
            }

            tracing::info!(
                "CLIENT {}: Resync attempt {}/{} for {} lost to a local edit",
                client_id,
                attempt,
                RESYNC_ATTEMPTS,
                document_id
            );
        }

        tracing::warn!(
            "CLIENT {}: Giving up resyncing {} for now, leaving it for the next broadcast",
            client_id,
            document_id
        );
        Ok(())
    }

    /// Reconcile a `get_document` reply with local state.
    ///
    /// A missing document (`DocumentNotFound`) is authoritative — the server
    /// checked both the user's own documents and the public set — so the local
    /// copy is soft-deleted. Any other error is transient and MUST leave local
    /// state untouched: deleting on a network blip destroys user data.
    /// Returns `true` when the document is settled (written, deleted, or
    /// deliberately left alone) and `false` when a concurrent local write won
    /// the compare-and-swap, meaning a retry against fresher server state is
    /// worthwhile.
    async fn apply_resync_result(
        db: &Arc<ClientDatabase>,
        client_id: Uuid,
        event_dispatcher: &Arc<EventDispatcher>,
        document_id: Uuid,
        result: ServerMessage,
    ) -> SyncResult<bool> {
        let (content, sync_revision, content_hash, deleted) = match result {
            ServerMessage::GetDocumentResponse {
                content,
                sync_revision,
                content_hash,
                deleted,
                ..
            } => (content, sync_revision, content_hash, deleted),
            ServerMessage::Error {
                code: ErrorCode::DocumentNotFound,
                ..
            } => {
                // A document that still owes a create has never been offered to
                // the server, so "not found" is the expected answer rather than
                // an authoritative deletion. Soft-deleting here would destroy
                // content that exists nowhere else.
                if db.has_queued_create(&document_id).await.unwrap_or(false) {
                    tracing::info!(
                        "CLIENT {}: Server does not have {} because its create is still queued, keeping the local copy",
                        client_id,
                        document_id
                    );
                    return Ok(true);
                }

                tracing::info!(
                    "CLIENT {}: Server does not have {}, soft-deleting locally",
                    client_id,
                    document_id
                );
                db.delete_document(&document_id).await?;
                db.mark_synced(&document_id).await?;
                event_dispatcher.emit_document_deleted(&document_id, EventOrigin::Remote);
                return Ok(true);
            }
            other => {
                tracing::warn!(
                    "CLIENT {}: Resync of {} failed transiently ({:?}), local state unchanged",
                    client_id,
                    document_id,
                    other
                );
                return Ok(true);
            }
        };

        if deleted {
            tracing::info!(
                "CLIENT {}: Resync of {} returned a tombstone, soft-deleting locally",
                client_id,
                document_id
            );
            db.delete_document(&document_id).await?;
            db.mark_synced(&document_id).await?;
            event_dispatcher.emit_document_deleted(&document_id, EventOrigin::Remote);
            return Ok(true);
        }

        // Read the status first: it is what distinguishes "no such row" from
        // "the row is there but the read failed".
        let status = db.get_sync_status(&document_id).await?;
        let local = match db.get_document(&document_id).await {
            Ok(doc) => Some(doc),
            Err(_) if status.is_none() => None,
            Err(e) => {
                // The row exists, so this is a read failure, not an absence.
                // Writing here would overwrite local state we never saw.
                tracing::warn!(
                    "CLIENT {}: Could not read {} for resync ({}), leaving it untouched",
                    client_id,
                    document_id,
                    e
                );
                return Ok(true);
            }
        };

        let Some(mut doc) = local else {
            // Genuinely new: status is None AND the row is absent, so there is
            // no local state to protect and an unconditional insert is safe.
            let doc = Document {
                id: document_id,
                user_id: None,
                content,
                sync_revision,
                content_hash: Some(content_hash),
                title: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                deleted_at: None,
                author_name: None,
                visibility: None,
                provenance: None,
            };
            db.save_document_with_status(&doc, Some(SyncStatus::Synced))
                .await?;
            event_dispatcher.emit_document_updated_with_attribution(
                &doc.id,
                &doc.content,
                doc.user_id.as_ref(),
                doc.author_name.as_deref(),
                doc.visibility.as_deref(),
                EventOrigin::Remote,
            );
            return Ok(true);
        };

        // Everything below writes conditionally on the state just read, so a
        // local edit landing between the read and the write loses the swap
        // instead of being silently overwritten.
        let expected = DocumentPreImage {
            sync_revision: doc.sync_revision,
            content: doc.content.clone(),
            sync_status: status.unwrap_or(SyncStatus::Conflict),
        };
        // A `Conflict` settled by a failed rebase holds the server's content and
        // has nothing queued, so there is nothing to protect: take the server's
        // state and keep the flag. Only a genuine local edit resolves a conflict.
        //
        // A `Conflict` settled by a REFUSED CREATE is the exception: it holds
        // purely local content the server has never seen, and its queue still
        // owes a create. That must be protected like any unsent edit, so an
        // unconsumed create counts as a local edit in its own right — with or
        // without a queued patch beside it.
        let has_local_edits = match expected.sync_status {
            SyncStatus::Pending => true,
            SyncStatus::Conflict => {
                db.has_queued_create(&document_id).await.unwrap_or(false)
                    || db
                        .get_queued_patch(&document_id)
                        .await
                        .ok()
                        .flatten()
                        .is_some()
            }
            _ => false,
        };

        let local_content = doc.content.clone();
        doc.sync_revision = sync_revision;
        doc.updated_at = chrono::Utc::now();

        let committed = if has_local_edits {
            // Keep the user's edits visible, but rebase them: the local copy now
            // sits on the server's revision, and the queued patch carries the
            // edits forward from the server's current content.
            doc.content_hash = None;
            db.rebase_pending_document_if_unchanged(
                &document_id,
                &local_content,
                sync_revision,
                &content,
                &content_hash,
                expected.sync_status,
                &expected,
            )
            .await?
        } else {
            doc.content = content;
            doc.content_hash = Some(content_hash);
            db.save_document_if_unchanged(&doc, settled_status(expected.sync_status), &expected)
                .await?
        };

        if !committed {
            tracing::warn!(
                "CLIENT {}: Resync of {} raced a local edit",
                client_id,
                document_id
            );
            return Ok(false);
        }

        tracing::info!(
            "CLIENT {}: Resynced {} to server revision {} (rebased local edits: {})",
            client_id,
            document_id,
            sync_revision,
            has_local_edits
        );

        event_dispatcher.emit_document_updated_with_attribution(
            &doc.id,
            &doc.content,
            doc.user_id.as_ref(),
            doc.author_name.as_deref(),
            doc.visibility.as_deref(),
            EventOrigin::Remote,
        );

        Ok(true)
    }

    async fn handle_server_message(
        msg: ServerMessage,
        db: &Arc<ClientDatabase>,
        client_id: Uuid,
        event_dispatcher: &Arc<EventDispatcher>,
        source: &ResyncSource,
    ) -> SyncResult<()> {
        match msg {
            ServerMessage::DocumentUpdated { patch } => {
                let document_id = patch.document_id;
                tracing::info!(
                    "CLIENT {}: Received DocumentUpdated for doc {} (revision {})",
                    client_id,
                    document_id,
                    patch.sync_revision
                );

                let Ok(mut doc) = db.get_document(&document_id).await else {
                    tracing::info!(
                        "CLIENT {}: Patch for unknown doc {}, resyncing",
                        client_id,
                        document_id
                    );
                    return Self::resync_document(
                        db,
                        source,
                        client_id,
                        event_dispatcher,
                        document_id,
                    )
                    .await;
                };

                // Already at or past this revision: a duplicate or our own
                // broadcast echoed back. Re-applying would corrupt the doc.
                if doc.sync_revision >= patch.sync_revision {
                    tracing::info!(
                        "CLIENT {}: Dropping duplicate patch for {} (local revision {} >= {})",
                        client_id,
                        document_id,
                        doc.sync_revision,
                        patch.sync_revision
                    );
                    return Ok(());
                }

                // Unsent local edits, or a gap in the revision stream: the
                // patch's base is not what we hold, so never blind-apply it.
                //
                // A `Conflict` settled by a failed rebase holds the SERVER's
                // content — only the flag differs — so a contiguous patch
                // applies to it exactly as safely as to a `Synced` document. The
                // flag is preserved on the write below: only a genuine local
                // edit resolves a conflict.
                //
                // A `Conflict` settled by a refused create holds purely local
                // content instead, so this would not be safe for it — but the
                // server cannot broadcast a patch for a document it does not
                // have, so no such patch can reach this branch.
                let status = db.get_sync_status(&document_id).await?;
                let appliable = matches!(
                    status,
                    Some(SyncStatus::Synced) | Some(SyncStatus::Conflict)
                );
                if !appliable || doc.sync_revision + 1 != patch.sync_revision {
                    tracing::info!(
                        "CLIENT {}: Cannot apply patch for {} (status {:?}, local revision {}, patch revision {}), resyncing",
                        client_id,
                        document_id,
                        status,
                        doc.sync_revision,
                        patch.sync_revision
                    );
                    return Self::resync_document(
                        db,
                        source,
                        client_id,
                        event_dispatcher,
                        document_id,
                    )
                    .await;
                }

                let mut new_content = doc.content.clone();
                if apply_patch(&mut new_content, &patch.patch).is_err()
                    || calculate_checksum(&new_content) != patch.content_hash
                {
                    tracing::warn!(
                        "CLIENT {}: Patch diverged for {}, resyncing",
                        client_id,
                        document_id
                    );
                    return Self::resync_document(
                        db,
                        source,
                        client_id,
                        event_dispatcher,
                        document_id,
                    )
                    .await;
                }

                // Commit conditionally on the state the guard just verified: a
                // local update landing since then must not be overwritten. The
                // status is carried through unchanged so a document the user
                // still has to resolve does not silently become `Synced`.
                let status = status.unwrap_or(SyncStatus::Conflict);
                let expected = DocumentPreImage {
                    sync_revision: doc.sync_revision,
                    content: doc.content.clone(),
                    sync_status: status,
                };

                doc.content = new_content;
                doc.sync_revision = patch.sync_revision;
                doc.content_hash = Some(patch.content_hash);
                doc.updated_at = chrono::Utc::now();

                if !db
                    .save_document_if_unchanged(&doc, status, &expected)
                    .await?
                {
                    tracing::warn!(
                        "CLIENT {}: Patch for {} raced a local edit, resyncing instead",
                        client_id,
                        document_id
                    );
                    return Self::resync_document(
                        db,
                        source,
                        client_id,
                        event_dispatcher,
                        document_id,
                    )
                    .await;
                }

                event_dispatcher.emit_document_updated_with_attribution(
                    &doc.id,
                    &doc.content,
                    doc.user_id.as_ref(),
                    doc.author_name.as_deref(),
                    doc.visibility.as_deref(),
                    EventOrigin::Remote,
                );
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
                            event_dispatcher.emit_document_updated_with_attribution(
                                &document.id,
                                &document.content,
                                document.user_id.as_ref(),
                                document.author_name.as_deref(),
                                document.visibility.as_deref(),
                                EventOrigin::Remote,
                            );
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
                        event_dispatcher.emit_document_created_with_attribution(
                            &document.id,
                            &document.content,
                            document.user_id.as_ref(),
                            document.author_name.as_deref(),
                            document.visibility.as_deref(),
                            EventOrigin::Remote,
                        );
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
                event_dispatcher.emit_document_deleted(&document_id, EventOrigin::Remote);
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
                            event_dispatcher.emit_document_updated_with_attribution(
                                &document.id,
                                &document.content,
                                document.user_id.as_ref(),
                                document.author_name.as_deref(),
                                document.visibility.as_deref(),
                                EventOrigin::Remote,
                            );
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
                        event_dispatcher.emit_document_created_with_attribution(
                            &document.id,
                            &document.content,
                            document.user_id.as_ref(),
                            document.author_name.as_deref(),
                            document.visibility.as_deref(),
                            EventOrigin::Remote,
                        );
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
                author_name,
                visibility,
                provenance,
            } => {
                if success {
                    tracing::info!(
                        "CLIENT {}: Document creation confirmed by server: {}",
                        client_id,
                        document_id
                    );
                    db.update_attribution(
                        &document_id,
                        author_name.clone(),
                        visibility.clone(),
                        provenance.clone(),
                    )
                    .await?;
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
                    event_dispatcher.emit_sync_error(
                        ReplicantErrorCode::Unknown,
                        &format!("Create failed: {}", error.as_deref().unwrap_or("unknown")),
                    );
                }
            }

            ServerMessage::DocumentUpdatedResponse {
                document_id,
                success,
                error,
                sync_revision,
                ..
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
                    event_dispatcher.emit_sync_error(
                        ReplicantErrorCode::Unknown,
                        &format!("Update failed: {}", error.as_deref().unwrap_or("unknown")),
                    );
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
                    event_dispatcher.emit_sync_error(
                        ReplicantErrorCode::Unknown,
                        &format!("Delete failed: {}", error.as_deref().unwrap_or("unknown")),
                    );
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

        // Clear the pending uploads (we'll re-add them during retry).
        // Clearing without an ack means nothing else drains the deferred queue,
        // so drain it here or those messages wait for an unrelated document's ack.
        self.pending_uploads.lock().await.clear();
        if let Err(e) = Self::process_deferred_messages(
            &self.deferred_messages,
            &self.db,
            self.client_id,
            &self.event_dispatcher,
            &self.pending_uploads,
            &ResyncSource::socket(&self.ws_client),
        )
        .await
        {
            tracing::error!(
                "CLIENT {}: Error draining deferred messages before retry: {}",
                self.client_id,
                e
            );
        }

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

        // A queued create outranks a queued patch; see `classify_pending_upload`.
        let (operation_type, message) = match classify_pending_upload(&self.db, &document.id).await
        {
            PendingUploadKind::Update {
                patch,
                old_content_hash,
            } => {
                tracing::info!(
                    "CLIENT {}: Sending UPDATE with queued patch for doc {}",
                    self.client_id,
                    document.id
                );

                use replicant_core::models::DocumentPatch;
                use replicant_core::patches::calculate_checksum;

                // Use the stored old content hash, or calculate from current content as fallback
                let content_hash =
                    old_content_hash.unwrap_or_else(|| calculate_checksum(&document.content));

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
            PendingUploadKind::Create => {
                tracing::info!(
                    "CLIENT {}: Sending CREATE for doc {} (the server does not hold it yet)",
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
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        if !self.sync_enabled {
            tracing::info!(
                "CLIENT {}: sync disabled (no credentials or identity not adopted) — \
                 skipping reconnection monitor",
                self.client_id
            );
            return;
        }
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
        let upload_retry = self.upload_retry.clone();
        let stopped = self.stopped.clone();

        tracing::info!(
            "🔄 CLIENT {}: Starting continuous reconnection monitor (5-second intervals)",
            client_id
        );

        tokio::spawn(async move {
            const RECONNECTION_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
            let mut connection_attempts = 0;

            loop {
                if shutdown_requested(&stopped, &is_connected) {
                    tracing::info!(
                        "CLIENT {}: Client shut down, reconnection monitor stopping",
                        client_id
                    );
                    break;
                }
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
                            if shutdown_requested(&stopped, &is_connected) {
                                break;
                            }
                            tracing::info!(
                                "✅ CLIENT {}: Reconnection successful after {} attempts!",
                                client_id,
                                connection_attempts
                            );
                            connection_attempts = 0;

                            // Update the client
                            {
                                let mut guard = ws_client.lock().await;
                                if shutdown_requested(&stopped, &is_connected) {
                                    break;
                                }
                                *guard = Some(new_client);
                            }
                            is_connected.store(true, Ordering::Relaxed);
                            // Shutdown may have run while the lock was held.
                            if shutdown_requested(&stopped, &is_connected) {
                                break;
                            }

                            // Reset ping timer on successful connection
                            *last_ping_time.lock().await = Some(Instant::now());

                            if shutdown_requested(&stopped, &is_connected) {
                                break;
                            }
                            event_dispatcher.emit_connection_succeeded(&server_url);

                            if shutdown_requested(&stopped, &is_connected) {
                                break;
                            }
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
                            let upload_retry_clone = upload_retry.clone();
                            let ws_client_for_handler = ws_client.clone();
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
                                        &upload_retry_clone,
                                        &ResyncSource::socket(&ws_client_for_handler),
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

                            // Those uploads will never be acked, so nothing else
                            // would drain messages deferred behind them.
                            if let Err(e) = Self::process_deferred_messages(
                                &deferred_messages,
                                &db,
                                client_id,
                                &event_dispatcher,
                                &pending_uploads,
                                &ResyncSource::socket(&ws_client),
                            )
                            .await
                            {
                                tracing::error!(
                                    "CLIENT {}: Error draining deferred messages after reconnection: {}",
                                    client_id,
                                    e
                                );
                            }

                            // Trigger pending sync on the real sync engine via channel
                            // This will upload any pending documents and THEN request full sync
                            tracing::info!(
                                "📤 CLIENT {}: Triggering post-reconnection sync on real engine",
                                client_id
                            );

                            if shutdown_requested(&stopped, &is_connected) {
                                break;
                            }
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
                        // A queued create outranks a queued patch; see
                        // `classify_pending_upload`.
                        match classify_pending_upload(db, &pending_info.id).await {
                            PendingUploadKind::Update {
                                patch: json_patch,
                                old_content_hash: old_hash_opt,
                            } => {
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
                            PendingUploadKind::Create => {
                                tracing::info!(
                                    "CLIENT {}: Doc {} has not reached the server - using CreateDocument",
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

/// The create-vs-update decision the upload paths share.
#[cfg(test)]
mod upload_classification_tests {
    use super::*;
    use replicant_core::patches::{apply_patch, create_patch};
    use replicant_core::protocol::ChangeEventType;
    use serde_json::json;

    async fn test_db() -> ClientDatabase {
        let db = ClientDatabase::new(":memory:").await.unwrap();
        db.run_migrations().await.unwrap();
        db
    }

    fn doc_with(id: Uuid, content: serde_json::Value) -> Document {
        Document {
            id,
            user_id: Some(Uuid::new_v4()),
            content,
            sync_revision: 1,
            content_hash: None,
            title: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            deleted_at: None,
            author_name: None,
            visibility: None,
            provenance: None,
        }
    }

    /// One local edit, exactly as `Client::update_document` performs it.
    async fn edit(db: &ClientDatabase, id: Uuid, new_content: serde_json::Value) {
        let mut doc = db.get_document(&id).await.unwrap();
        let old_content = doc.content.clone();
        let patch = create_patch(&old_content, &new_content).unwrap();
        doc.content = new_content;
        db.save_document_and_queue_patch(&doc, &patch, ChangeEventType::Update, Some(&old_content))
            .await
            .unwrap();
    }

    /// The DEV-1040 case: created offline, then edited offline twice. The queued
    /// update must not win, or the server is asked to patch a document it has
    /// never seen and answers `not_found` forever.
    #[tokio::test]
    async fn a_document_created_offline_uploads_as_a_create_carrying_its_edits() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        db.save_new_document_and_queue_create(&doc_with(id, json!({"title": "draft"})))
            .await
            .unwrap();

        edit(&db, id, json!({"title": "draft", "a": 1})).await;
        let final_content = json!({"title": "draft", "a": 1, "b": 2});
        edit(&db, id, final_content.clone()).await;

        assert!(
            matches!(
                classify_pending_upload(&db, &id).await,
                PendingUploadKind::Create
            ),
            "an unconsumed create must outrank the queued patch"
        );
        assert_eq!(
            db.get_document(&id).await.unwrap().content,
            final_content,
            "the create sends current content, which must hold both edits"
        );
    }

    /// The regression guard: a document the server already acknowledged still
    /// uploads its edits as a cumulative update.
    #[tokio::test]
    async fn a_document_the_server_holds_uploads_as_an_update() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let acknowledged = json!({"title": "draft"});
        db.save_new_document_and_queue_create(&doc_with(id, acknowledged.clone()))
            .await
            .unwrap();
        db.remove_from_sync_queue(&id).await.unwrap(); // the create ack

        edit(&db, id, json!({"title": "draft", "a": 1})).await;
        let final_content = json!({"title": "draft", "a": 1, "b": 2});
        edit(&db, id, final_content.clone()).await;

        let PendingUploadKind::Update { patch, .. } = classify_pending_upload(&db, &id).await
        else {
            panic!("an acknowledged document must upload as an update");
        };

        let mut replayed = acknowledged;
        apply_patch(&mut replayed, &patch).unwrap();
        assert_eq!(
            replayed, final_content,
            "the update must still be one cumulative patch off the acknowledged content"
        );
    }

    /// A document that is pending with nothing queued at all — a pre-upgrade
    /// database, or a create whose row predates this fix — still reads as a
    /// create, exactly as before.
    #[tokio::test]
    async fn a_pending_document_with_an_empty_queue_uploads_as_a_create() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        db.save_document_with_status(
            &doc_with(id, json!({"title": "draft"})),
            Some(SyncStatus::Pending),
        )
        .await
        .unwrap();

        assert!(matches!(
            classify_pending_upload(&db, &id).await,
            PendingUploadKind::Create
        ));
    }
}

#[cfg(test)]
mod broadcast_guard_tests {
    use super::*;
    use crate::events::{EventOrigin, SyncEvent};
    use replicant_core::models::ServerDocumentPatch;
    use replicant_core::patches::calculate_checksum;
    use replicant_core::protocol::ErrorCode;
    use serde_json::{json, Value};

    async fn test_db() -> Arc<ClientDatabase> {
        let db = ClientDatabase::new(":memory:").await.unwrap();
        db.run_migrations().await.unwrap();
        Arc::new(db)
    }

    fn dispatcher() -> Arc<EventDispatcher> {
        Arc::new(EventDispatcher::new())
    }

    /// A resync source that records attempts and returns nothing, so a test can
    /// assert a resync WAS attempted rather than inferring it from the absence
    /// of a write.
    fn recording_source() -> (ResyncSource, Arc<CannedResync>) {
        canned_source(Vec::new())
    }

    /// A resync source that hands out `replies` one per fetch attempt.
    fn canned_source(replies: Vec<ServerMessage>) -> (ResyncSource, Arc<CannedResync>) {
        let canned = Arc::new(CannedResync {
            attempts: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into()),
        });
        (ResyncSource::Canned(canned.clone()), canned)
    }

    /// An `UploadRetry` plus the receiving end of its trigger channel.
    fn retry_probe() -> (UploadRetry, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel(10);
        (UploadRetry::new(tx), rx)
    }

    /// A rejected update ack. `current` carries the server's state at rejection
    /// time, which the server only sends for `hash_mismatch`.
    fn update_rejected(
        document_id: Uuid,
        reason: &str,
        current: Option<(i64, Value, String)>,
    ) -> ServerMessage {
        let (current_revision, current_content, current_hash) = match current {
            Some((revision, content, hash)) => (Some(revision), Some(content), Some(hash)),
            None => (None, None, None),
        };
        ServerMessage::DocumentUpdatedResponse {
            document_id,
            success: false,
            error: Some(format!("server rejected update: {}", reason)),
            sync_revision: None,
            reason: Some(reason.to_string()),
            current_revision,
            current_content,
            current_hash,
        }
    }

    /// Deliver an upload ack through the tracking handler with the document's
    /// upload in flight, exactly as the message loop would.
    async fn deliver_ack(
        db: &Arc<ClientDatabase>,
        events: &Arc<EventDispatcher>,
        upload_retry: &UploadRetry,
        id: Uuid,
        ack: ServerMessage,
    ) {
        let (source, _canned) = recording_source();
        deliver_ack_from(db, events, upload_retry, id, ack, &source).await;
    }

    /// `deliver_ack` against a chosen resync source, for acks whose recovery
    /// path fetches the document from the server.
    async fn deliver_ack_from(
        db: &Arc<ClientDatabase>,
        events: &Arc<EventDispatcher>,
        upload_retry: &UploadRetry,
        id: Uuid,
        ack: ServerMessage,
        source: &ResyncSource,
    ) {
        let (pending_uploads, deferred_messages, notifier, protection) = tracking_args();
        pending_uploads.lock().await.insert(id, in_flight_upload());

        Client::handle_server_message_with_tracking(
            ack,
            db,
            Uuid::new_v4(),
            events,
            &pending_uploads,
            &notifier,
            &protection,
            &deferred_messages,
            upload_retry,
            source,
        )
        .await
        .unwrap();

        assert!(
            pending_uploads.lock().await.is_empty(),
            "an ack must settle the upload before anything is resent"
        );
    }

    /// A create the server refused.
    fn create_rejected(document_id: Uuid, error: &str) -> ServerMessage {
        ServerMessage::DocumentCreatedResponse {
            document_id,
            success: false,
            error: Some(error.to_string()),
            author_name: None,
            visibility: None,
            provenance: None,
        }
    }

    async fn queued_base_hash(db: &Arc<ClientDatabase>, id: &Uuid) -> Option<String> {
        db.get_queued_patch(id).await.unwrap().and_then(|(_, h)| h)
    }

    /// The stored content text, not the parsed value: the compare-and-swap
    /// compares text, so "unchanged" has to be asserted at that level.
    async fn raw_content(db: &Arc<ClientDatabase>, id: &Uuid) -> String {
        sqlx::query("SELECT content FROM documents WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .try_get("content")
            .unwrap()
    }

    async fn resync_attempts(canned: &Arc<CannedResync>) -> Vec<Uuid> {
        canned.attempts.lock().await.clone()
    }

    fn make_doc(id: Uuid, content: Value, sync_revision: i64) -> Document {
        Document {
            id,
            user_id: Some(Uuid::new_v4()),
            content,
            sync_revision,
            content_hash: None,
            title: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            deleted_at: None,
            author_name: None,
            visibility: None,
            provenance: None,
        }
    }

    async fn seed(db: &Arc<ClientDatabase>, doc: &Document, status: SyncStatus) {
        db.save_document_with_status(doc, Some(status))
            .await
            .unwrap();
    }

    /// Seed a document the way a real local edit does: save it Pending AND
    /// queue its patch, so the sync_queue is in its production shape.
    async fn seed_pending_edit(
        db: &Arc<ClientDatabase>,
        id: Uuid,
        base: &Value,
        edited: &Value,
        sync_revision: i64,
    ) {
        seed(
            db,
            &make_doc(id, base.clone(), sync_revision),
            SyncStatus::Synced,
        )
        .await;
        let doc = make_doc(id, edited.clone(), sync_revision);
        db.save_document_and_queue_patch(
            &doc,
            &create_patch(base, edited).unwrap(),
            replicant_core::protocol::ChangeEventType::Update,
            Some(base),
        )
        .await
        .unwrap();
    }

    async fn queued_update_rows(db: &Arc<ClientDatabase>, id: &Uuid) -> i64 {
        sqlx::query(
            "SELECT COUNT(*) as c FROM sync_queue WHERE document_id = ? AND operation_type = 'update'",
        )
        .bind(id.to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .try_get("c")
        .unwrap()
    }

    async fn deleted_at(db: &Arc<ClientDatabase>, id: &Uuid) -> Option<String> {
        sqlx::query("SELECT deleted_at FROM documents WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .try_get("deleted_at")
            .unwrap()
    }

    fn server_patch(
        document_id: Uuid,
        before: &Value,
        after: &Value,
        sync_revision: i64,
    ) -> ServerDocumentPatch {
        ServerDocumentPatch {
            document_id,
            patch: create_patch(before, after).unwrap(),
            sync_revision,
            content_hash: calculate_checksum(after),
        }
    }

    /// Deliver a broadcast patch and report which documents the handler tried
    /// to resync as a result.
    async fn deliver(
        db: &Arc<ClientDatabase>,
        events: &Arc<EventDispatcher>,
        patch: ServerDocumentPatch,
    ) -> Vec<Uuid> {
        let (source, canned) = recording_source();
        Client::handle_server_message(
            ServerMessage::DocumentUpdated { patch },
            db,
            Uuid::new_v4(),
            events,
            &source,
        )
        .await
        .unwrap();
        resync_attempts(&canned).await
    }

    #[tokio::test]
    async fn contiguous_broadcast_with_matching_hash_applies() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let before = json!({"title": "A"});
        let after = json!({"title": "B"});
        seed(&db, &make_doc(id, before.clone(), 5), SyncStatus::Synced).await;

        let resynced = deliver(&db, &dispatcher(), server_patch(id, &before, &after, 6)).await;

        assert!(resynced.is_empty(), "a clean apply must not resync");
        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(doc.content, after);
        assert_eq!(doc.sync_revision, 6);
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Synced)
        );
    }

    #[tokio::test]
    async fn duplicate_delivery_of_same_revision_is_a_noop() {
        let db = test_db().await;
        let events = dispatcher();
        let id = Uuid::new_v4();
        let before = json!({"title": "A"});
        let after = json!({"title": "B"});
        seed(&db, &make_doc(id, before.clone(), 5), SyncStatus::Synced).await;

        let patch = server_patch(id, &before, &after, 6);
        deliver(&db, &events, patch.clone()).await;
        let resynced = deliver(&db, &events, patch).await;

        assert!(resynced.is_empty(), "a duplicate must drop, not resync");
        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(doc.content, after, "second delivery must not re-patch");
        assert_eq!(doc.sync_revision, 6);
    }

    #[tokio::test]
    async fn self_broadcast_at_or_below_local_revision_is_dropped() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let before = json!({"title": "A"});
        let after = json!({"title": "B"});
        // Local already advanced to 6 (our own update was confirmed); the
        // server echoes our own broadcast back at 6.
        seed(&db, &make_doc(id, after.clone(), 6), SyncStatus::Synced).await;

        let resynced = deliver(&db, &dispatcher(), server_patch(id, &before, &after, 6)).await;

        assert!(resynced.is_empty(), "a self-echo must drop, not resync");
        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(doc.content, after);
        assert_eq!(doc.sync_revision, 6);
    }

    #[tokio::test]
    async fn revision_gap_does_not_blind_apply() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let local = json!({"title": "A"});
        seed(&db, &make_doc(id, local.clone(), 5), SyncStatus::Synced).await;

        // u1 was missed; only u2's broadcast (revision 7) arrives.
        let skipped_base = json!({"title": "B"});
        let resynced = deliver(
            &db,
            &dispatcher(),
            server_patch(id, &skipped_base, &json!({"title": "C"}), 7),
        )
        .await;

        assert_eq!(resynced, vec![id], "the gap must trigger a resync");
        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(doc.content, local, "gap must resync, never frankenpatch");
        assert_eq!(doc.sync_revision, 5);
    }

    #[tokio::test]
    async fn pending_local_edits_are_never_overwritten_by_a_broadcast() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let local = json!({"title": "local edit"});
        seed(&db, &make_doc(id, local.clone(), 5), SyncStatus::Pending).await;

        let resynced = deliver(
            &db,
            &dispatcher(),
            server_patch(id, &json!({"title": "A"}), &json!({"title": "B"}), 6),
        )
        .await;

        assert_eq!(
            resynced,
            vec![id],
            "pending local edits must trigger a resync"
        );
        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(doc.content, local);
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Pending)
        );
    }

    #[tokio::test]
    async fn hash_mismatch_after_apply_does_not_commit_the_patch() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let before = json!({"title": "A"});
        seed(&db, &make_doc(id, before.clone(), 5), SyncStatus::Synced).await;

        let mut patch = server_patch(id, &before, &json!({"title": "B"}), 6);
        patch.content_hash =
            "0000000000000000000000000000000000000000000000000000000000000000".to_string();
        deliver(&db, &dispatcher(), patch).await;

        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(doc.content, before, "diverged patch must resync, not apply");
        assert_eq!(doc.sync_revision, 5);
    }

    #[tokio::test]
    async fn resync_response_replaces_a_synced_local_document() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed(
            &db,
            &make_doc(id, json!({"title": "stale"}), 5),
            SyncStatus::Synced,
        )
        .await;

        let fresh = json!({"title": "fresh", "n": 2});
        Client::apply_resync_result(
            &db,
            Uuid::new_v4(),
            &dispatcher(),
            id,
            ServerMessage::GetDocumentResponse {
                id,
                content: fresh.clone(),
                sync_revision: 9,
                content_hash: calculate_checksum(&fresh),
                deleted: false,
            },
        )
        .await
        .unwrap();

        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(doc.content, fresh);
        assert_eq!(doc.sync_revision, 9);
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Synced)
        );
    }

    #[tokio::test]
    async fn resync_rebases_pending_local_edits_onto_the_fresh_base() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let local = json!({"title": "stale", "mine": true});
        // Seed the way production does: a local edit saves the document AND
        // queues its patch, so the rebase below has a prior row to replace.
        seed_pending_edit(&db, id, &json!({"title": "stale"}), &local, 5).await;

        let fresh = json!({"title": "fresh"});
        let fresh_hash = calculate_checksum(&fresh);
        Client::apply_resync_result(
            &db,
            Uuid::new_v4(),
            &dispatcher(),
            id,
            ServerMessage::GetDocumentResponse {
                id,
                content: fresh.clone(),
                sync_revision: 9,
                content_hash: fresh_hash.clone(),
                deleted: false,
            },
        )
        .await
        .unwrap();

        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(doc.content, local, "local edits must survive the rebase");
        assert_eq!(doc.sync_revision, 9, "base revision adopted from server");
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Pending)
        );

        assert_eq!(
            queued_update_rows(&db, &id).await,
            1,
            "the rebase must replace the queued patch, not append a second row"
        );
        let (patch, base_hash) = db.get_queued_patch(&id).await.unwrap().unwrap();
        assert_eq!(base_hash.as_deref(), Some(fresh_hash.as_str()));
        let mut replayed = fresh;
        apply_patch(&mut replayed, &patch).unwrap();
        assert_eq!(replayed, local, "queued patch must rebuild local content");
    }

    /// Build the argument bundle `handle_server_message_with_tracking` needs.
    fn tracking_args() -> (
        Arc<Mutex<HashMap<Uuid, PendingUpload>>>,
        Arc<Mutex<Vec<ServerMessage>>>,
        Arc<Notify>,
        Arc<AtomicBool>,
    ) {
        (
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn in_flight_upload() -> PendingUpload {
        PendingUpload {
            operation_type: UploadType::Update,
            sent_at: Instant::now(),
        }
    }

    #[tokio::test]
    async fn a_failed_ack_clears_the_pending_upload_and_drains_the_queue() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let before = json!({"title": "A"});
        let after = json!({"title": "B"});
        seed_pending_edit(&db, id, &before, &after, 5).await;

        let (pending_uploads, deferred_messages, notifier, protection) = tracking_args();
        pending_uploads.lock().await.insert(id, in_flight_upload());
        let (source, canned) = recording_source();

        // The echo arrives while the upload is in flight, so it defers.
        Client::handle_server_message_with_tracking(
            ServerMessage::DocumentUpdated {
                patch: server_patch(id, &before, &after, 6),
            },
            &db,
            Uuid::new_v4(),
            &dispatcher(),
            &pending_uploads,
            &notifier,
            &protection,
            &deferred_messages,
            &retry_probe().0,
            &source,
        )
        .await
        .unwrap();
        assert_eq!(deferred_messages.lock().await.len(), 1);

        // The server rejects the upload. The entry must still be cleared and
        // the queue drained, or this document defers forever.
        db.update_sync_revision(&id, 6).await.unwrap();
        db.mark_synced(&id).await.unwrap();
        let (upload_retry, mut retry_rx) = retry_probe();
        Client::handle_server_message_with_tracking(
            update_rejected(id, "server_error", None),
            &db,
            Uuid::new_v4(),
            &dispatcher(),
            &pending_uploads,
            &notifier,
            &protection,
            &deferred_messages,
            &upload_retry,
            &source,
        )
        .await
        .unwrap();

        assert!(
            retry_rx.try_recv().is_ok(),
            "a failed ack must schedule an upload retry pass"
        );

        assert!(
            pending_uploads.lock().await.is_empty(),
            "a failed ack must clear the pending upload"
        );
        assert!(
            deferred_messages.lock().await.is_empty(),
            "a failed ack must drain the deferred queue"
        );
        assert!(
            resync_attempts(&canned).await.is_empty(),
            "the replayed echo must drop as a duplicate, not resync"
        );
    }

    #[tokio::test]
    async fn deferred_echoes_dedupe_and_cannot_evict_a_sync_document() {
        let db = test_db().await;
        let echo_id = Uuid::new_v4();
        let sync_id = Uuid::new_v4();
        let before = json!({"title": "A"});
        seed_pending_edit(&db, echo_id, &before, &json!({"title": "B"}), 5).await;

        let (pending_uploads, deferred_messages, notifier, protection) = tracking_args();
        {
            let mut uploads = pending_uploads.lock().await;
            uploads.insert(echo_id, in_flight_upload());
            uploads.insert(sync_id, in_flight_upload());
        }
        let (source, _canned) = recording_source();

        // A SyncDocument for another document gets deferred first.
        let mut sync_doc = make_doc(sync_id, json!({"title": "sync me"}), 2);
        sync_doc.title = None;
        db.save_document_with_status(&sync_doc, Some(SyncStatus::Synced))
            .await
            .unwrap();
        Client::handle_server_message_with_tracking(
            ServerMessage::SyncDocument {
                document: sync_doc.clone(),
            },
            &db,
            Uuid::new_v4(),
            &dispatcher(),
            &pending_uploads,
            &notifier,
            &protection,
            &deferred_messages,
            &retry_probe().0,
            &source,
        )
        .await
        .unwrap();

        // Then a burst of echoes for the other document.
        for revision in 6..206 {
            Client::handle_server_message_with_tracking(
                ServerMessage::DocumentUpdated {
                    patch: server_patch(echo_id, &before, &json!({"n": revision}), revision),
                },
                &db,
                Uuid::new_v4(),
                &dispatcher(),
                &pending_uploads,
                &notifier,
                &protection,
                &deferred_messages,
                &retry_probe().0,
                &source,
            )
            .await
            .unwrap();
        }

        let queue = deferred_messages.lock().await;
        assert_eq!(
            queue.len(),
            2,
            "200 echoes for one document must collapse to a single entry"
        );
        assert!(
            queue.iter().any(
                |m| matches!(m, ServerMessage::SyncDocument { document } if document.id == sync_id)
            ),
            "echo pressure must not evict the SyncDocument"
        );
        // The surviving echo is the newest one.
        let latest = queue
            .iter()
            .find_map(|m| match m {
                ServerMessage::DocumentUpdated { patch } => Some(patch.sync_revision),
                _ => None,
            })
            .unwrap();
        assert_eq!(latest, 205, "the newest echo must replace older ones");
    }

    #[tokio::test]
    async fn a_document_read_failure_during_resync_writes_nothing() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let local = json!({"title": "unsent edit"});
        seed(&db, &make_doc(id, local.clone(), 5), SyncStatus::Pending).await;

        // A row whose content is not valid JSON: the status reads back fine
        // (Pending) but parsing the document fails, which must NOT be mistaken
        // for "no such document".
        sqlx::query("UPDATE documents SET content = 'not json' WHERE id = ?")
            .bind(id.to_string())
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(db.get_document(&id).await.is_err(), "the read must fail");

        let fresh = json!({"title": "server"});
        let settled = Client::apply_resync_result(
            &db,
            Uuid::new_v4(),
            &dispatcher(),
            id,
            ServerMessage::GetDocumentResponse {
                id,
                content: fresh.clone(),
                sync_revision: 9,
                content_hash: calculate_checksum(&fresh),
                deleted: false,
            },
        )
        .await
        .unwrap();

        assert!(settled, "a read failure must not trigger a retry loop");
        let raw: String = sqlx::query("SELECT content FROM documents WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .try_get("content")
            .unwrap();
        assert_eq!(
            raw, "not json",
            "nothing may be written after a read failure"
        );
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Pending),
            "the pending edit must survive"
        );
    }

    #[tokio::test]
    async fn a_redeferred_message_never_overwrites_a_newer_queued_one() {
        let client_id = Uuid::new_v4();
        let queue = Arc::new(Mutex::new(Vec::new()));
        let id = Uuid::new_v4();

        // A newer SyncDocument landed from another producer task while an older
        // copy was in flight back to the queue.
        Client::defer_message(
            &queue,
            client_id,
            ServerMessage::SyncDocument {
                document: make_doc(id, json!({"title": "newer"}), 9),
            },
        )
        .await;

        Client::redefer_message(
            &queue,
            client_id,
            ServerMessage::SyncDocument {
                document: make_doc(id, json!({"title": "older"}), 4),
            },
        )
        .await;

        let queued = queue.lock().await;
        assert_eq!(queued.len(), 1);
        match &queued[0] {
            ServerMessage::SyncDocument { document } => {
                assert_eq!(document.sync_revision, 9, "the newer entry must survive");
                assert_eq!(document.content, json!({"title": "newer"}));
            }
            other => panic!("expected SyncDocument, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn deferring_an_older_message_never_downgrades_a_newer_one() {
        let client_id = Uuid::new_v4();
        let queue = Arc::new(Mutex::new(Vec::new()));
        let id = Uuid::new_v4();
        let before = json!({"title": "A"});

        Client::defer_message(
            &queue,
            client_id,
            ServerMessage::DocumentUpdated {
                patch: server_patch(id, &before, &json!({"n": 9}), 9),
            },
        )
        .await;
        Client::defer_message(
            &queue,
            client_id,
            ServerMessage::DocumentUpdated {
                patch: server_patch(id, &before, &json!({"n": 4}), 4),
            },
        )
        .await;

        let queued = queue.lock().await;
        assert_eq!(queued.len(), 1);
        assert_eq!(Client::deferral_revision(&queued[0]), 9);
    }

    #[tokio::test]
    async fn a_resync_that_settles_first_time_only_fetches_once() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed(
            &db,
            &make_doc(id, json!({"title": "stale"}), 5),
            SyncStatus::Synced,
        )
        .await;

        let fresh = json!({"title": "server"});
        let (source, canned) = canned_source(vec![ServerMessage::GetDocumentResponse {
            id,
            content: fresh.clone(),
            sync_revision: 9,
            content_hash: calculate_checksum(&fresh),
            deleted: false,
        }]);

        Client::resync_document(&db, &source, Uuid::new_v4(), &dispatcher(), id)
            .await
            .unwrap();

        assert_eq!(
            resync_attempts(&canned).await,
            vec![id],
            "a swap that wins must not re-fetch"
        );
        assert_eq!(db.get_document(&id).await.unwrap().content, fresh);
    }

    #[tokio::test]
    async fn a_resync_whose_swap_keeps_losing_refetches_once_then_gives_up() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let local = json!({"title": "local"});
        seed(&db, &make_doc(id, local.clone(), 5), SyncStatus::Synced).await;

        // Store the content with whitespace serde_json would never emit. The
        // row still parses to `local`, so the resync reads that as its
        // pre-image, but the compare-and-swap binds the canonical form and can
        // never match the stored text — a permanently losing swap.
        sqlx::query(r#"UPDATE documents SET content = '{"title": "local"}' WHERE id = ?"#)
            .bind(id.to_string())
            .execute(&db.pool)
            .await
            .unwrap();

        let fresh = json!({"title": "server"});
        let reply = || ServerMessage::GetDocumentResponse {
            id,
            content: fresh.clone(),
            sync_revision: 9,
            content_hash: calculate_checksum(&fresh),
            deleted: false,
        };
        let (source, canned) = canned_source(vec![reply(), reply(), reply()]);

        Client::resync_document(&db, &source, Uuid::new_v4(), &dispatcher(), id)
            .await
            .unwrap();

        assert_eq!(
            resync_attempts(&canned).await,
            vec![id, id],
            "a lost swap must re-fetch exactly once, then give up"
        );

        let raw: String = sqlx::query("SELECT content FROM documents WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .try_get("content")
            .unwrap();
        assert_eq!(
            raw, r#"{"title": "local"}"#,
            "a losing swap must never write"
        );
    }

    #[tokio::test]
    async fn compare_and_swap_refuses_to_clobber_a_concurrent_local_edit() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let base = json!({"title": "A"});
        seed(&db, &make_doc(id, base.clone(), 5), SyncStatus::Synced).await;

        // What the guard verified before deciding to write.
        let expected = DocumentPreImage {
            sync_revision: 5,
            content: base.clone(),
            sync_status: SyncStatus::Synced,
        };

        // A local edit lands in the read-check-write window.
        let local = json!({"title": "local edit"});
        seed_pending_edit(&db, id, &base, &local, 5).await;

        let committed = db
            .save_document_if_unchanged(
                &make_doc(id, json!({"title": "B"}), 6),
                SyncStatus::Synced,
                &expected,
            )
            .await
            .unwrap();

        assert!(!committed, "the swap must lose to the concurrent edit");
        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(doc.content, local, "the local edit must survive");
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Pending)
        );
    }

    #[tokio::test]
    async fn unparseable_sync_status_fails_closed_as_pending() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed(&db, &make_doc(id, json!({}), 1), SyncStatus::Synced).await;
        // The CHECK constraint blocks a garbage status today; bypass it so the
        // read path's fallback is exercised against schema drift.
        // The PRAGMA is per-connection, so both statements share one.
        let mut conn = db.pool.acquire().await.unwrap();
        sqlx::query("PRAGMA ignore_check_constraints = ON")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("UPDATE documents SET sync_status = 'banana' WHERE id = ?")
            .bind(id.to_string())
            .execute(&mut *conn)
            .await
            .unwrap();
        drop(conn);

        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Pending),
            "an unknown status must read back as the only non-appliable, \
             non-destructive value: Synced and Conflict both authorise an apply"
        );

        // And prove it: a contiguous broadcast onto the drifted row resyncs
        // instead of applying, so unsent local edits cannot be overwritten.
        let resynced = deliver(
            &db,
            &dispatcher(),
            server_patch(id, &json!({}), &json!({"title": "from the server"}), 2),
        )
        .await;
        assert_eq!(
            resynced,
            vec![id],
            "a drifted status must route to a resync, never a blind apply"
        );
    }

    #[tokio::test]
    async fn echo_during_an_in_flight_upload_is_deferred_not_resynced() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let before = json!({"title": "A"});
        let after = json!({"title": "B"});
        seed_pending_edit(&db, id, &before, &after, 5).await;

        let pending_uploads = Arc::new(Mutex::new(HashMap::from([(
            id,
            PendingUpload {
                operation_type: UploadType::Update,
                sent_at: Instant::now(),
            },
        )])));
        let deferred_messages = Arc::new(Mutex::new(Vec::new()));
        let (source, canned) = recording_source();

        Client::handle_server_message_with_tracking(
            ServerMessage::DocumentUpdated {
                patch: server_patch(id, &before, &after, 6),
            },
            &db,
            Uuid::new_v4(),
            &dispatcher(),
            &pending_uploads,
            &Arc::new(Notify::new()),
            &Arc::new(AtomicBool::new(false)),
            &deferred_messages,
            &retry_probe().0,
            &source,
        )
        .await
        .unwrap();

        assert!(
            resync_attempts(&canned).await.is_empty(),
            "an echo for an in-flight upload must not cost a resync"
        );
        assert_eq!(
            deferred_messages.lock().await.len(),
            1,
            "the echo must be deferred until the ack lands"
        );
        assert_eq!(
            queued_update_rows(&db, &id).await,
            1,
            "deferring must not add a queue row"
        );

        // After the ack, the deferred echo replays and hits the idempotency drop.
        db.update_sync_revision(&id, 6).await.unwrap();
        db.mark_synced(&id).await.unwrap();
        Client::process_deferred_messages(
            &deferred_messages,
            &db,
            Uuid::new_v4(),
            &dispatcher(),
            &pending_uploads,
            &source,
        )
        .await
        .unwrap();

        assert!(
            resync_attempts(&canned).await.is_empty(),
            "the replayed echo must drop as a duplicate"
        );
        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(doc.content, after);
        assert_eq!(doc.sync_revision, 6);
    }

    #[tokio::test]
    async fn resync_tombstone_soft_deletes_locally() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed(
            &db,
            &make_doc(id, json!({"title": "doomed"}), 5),
            SyncStatus::Synced,
        )
        .await;

        Client::apply_resync_result(
            &db,
            Uuid::new_v4(),
            &dispatcher(),
            id,
            ServerMessage::GetDocumentResponse {
                id,
                content: Value::Null,
                sync_revision: 6,
                content_hash: String::new(),
                deleted: true,
            },
        )
        .await
        .unwrap();

        assert!(deleted_at(&db, &id).await.is_some());
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Synced)
        );
    }

    #[tokio::test]
    async fn resync_not_found_soft_deletes_locally() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed(
            &db,
            &make_doc(id, json!({"title": "gone"}), 5),
            SyncStatus::Synced,
        )
        .await;

        Client::apply_resync_result(
            &db,
            Uuid::new_v4(),
            &dispatcher(),
            id,
            ServerMessage::Error {
                code: ErrorCode::DocumentNotFound,
                message: "Document not found".to_string(),
            },
        )
        .await
        .unwrap();

        assert!(deleted_at(&db, &id).await.is_some());
    }

    /// The exception to the rule above: a document that still owes a create has
    /// never been offered to the server, so `not_found` is the expected answer,
    /// not an authoritative deletion. Soft-deleting it would destroy the only
    /// copy of content the user just created.
    #[tokio::test]
    async fn resync_not_found_keeps_a_document_whose_create_is_still_queued() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let created = json!({"title": "never uploaded"});
        db.save_new_document_and_queue_create(&make_doc(id, created.clone(), 1))
            .await
            .unwrap();

        Client::apply_resync_result(
            &db,
            Uuid::new_v4(),
            &dispatcher(),
            id,
            ServerMessage::Error {
                code: ErrorCode::DocumentNotFound,
                message: "Document not found".to_string(),
            },
        )
        .await
        .unwrap();

        assert_eq!(
            deleted_at(&db, &id).await,
            None,
            "content the server has never seen must not be deleted for being absent"
        );
        assert_eq!(db.get_document(&id).await.unwrap().content, created);
        assert!(
            db.has_queued_create(&id).await.unwrap(),
            "the create is still owed"
        );
    }

    #[tokio::test]
    async fn resync_server_error_leaves_local_state_untouched() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let local = json!({"title": "unsent edit"});
        seed(&db, &make_doc(id, local.clone(), 5), SyncStatus::Pending).await;

        Client::apply_resync_result(
            &db,
            Uuid::new_v4(),
            &dispatcher(),
            id,
            ServerMessage::Error {
                code: ErrorCode::ServerError,
                message: "timeout".to_string(),
            },
        )
        .await
        .unwrap();

        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(doc.content, local);
        assert_eq!(doc.sync_revision, 5);
        assert!(deleted_at(&db, &id).await.is_none(), "must not delete");
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Pending)
        );
    }

    #[tokio::test]
    async fn resync_of_an_unknown_document_saves_it_as_synced() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let fresh = json!({"title": "brand new"});

        Client::apply_resync_result(
            &db,
            Uuid::new_v4(),
            &dispatcher(),
            id,
            ServerMessage::GetDocumentResponse {
                id,
                content: fresh.clone(),
                sync_revision: 3,
                content_hash: calculate_checksum(&fresh),
                deleted: false,
            },
        )
        .await
        .unwrap();

        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(doc.content, fresh);
        assert_eq!(doc.sync_revision, 3);
    }

    #[tokio::test]
    async fn remote_delete_of_a_pending_document_still_emits_deleted() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed(
            &db,
            &make_doc(id, json!({"title": "edited then deleted"}), 5),
            SyncStatus::Pending,
        )
        .await;

        // Remote deletes are user intent and stay unconditional: the pending
        // local edit is dropped and the host app is told the doc is gone.
        Client::handle_server_message(
            ServerMessage::DocumentDeleted { document_id: id },
            &db,
            Uuid::new_v4(),
            &dispatcher(),
            &recording_source().0,
        )
        .await
        .unwrap();

        assert!(deleted_at(&db, &id).await.is_some());
    }

    // ---------------------------------------------------------------------
    // hash_mismatch -> rebase and resend (Task 5)
    // ---------------------------------------------------------------------

    /// Collect emitted events. Emission only queues them; `drain_events` runs
    /// the pump, which must happen on the registering thread (here, the
    /// single-threaded test runtime).
    fn event_probe(events: &Arc<EventDispatcher>) -> Arc<std::sync::Mutex<Vec<SyncEvent>>> {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        events
            .register_rust_callback(move |event| sink.lock().unwrap().push(event))
            .unwrap();
        seen
    }

    fn drain_events(
        events: &Arc<EventDispatcher>,
        seen: &Arc<std::sync::Mutex<Vec<SyncEvent>>>,
    ) -> Vec<SyncEvent> {
        events.process_events().unwrap();
        seen.lock().unwrap().clone()
    }

    fn base_content() -> Value {
        json!({"title": "base", "referenceFrequency": 440.0})
    }

    #[tokio::test]
    async fn hash_mismatch_rebases_the_queued_patch_onto_the_server_content() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let base = base_content();
        // This client edited referenceFrequency; the winner edited title.
        let local = json!({"title": "base", "referenceFrequency": 441.0});
        seed_pending_edit(&db, id, &base, &local, 1).await;

        let server = json!({"title": "winner", "referenceFrequency": 440.0});
        let server_hash = calculate_checksum(&server);
        let (upload_retry, mut retry_rx) = retry_probe();

        deliver_ack(
            &db,
            &dispatcher(),
            &upload_retry,
            id,
            update_rejected(
                id,
                "hash_mismatch",
                Some((2, server.clone(), server_hash.clone())),
            ),
        )
        .await;

        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(
            doc.content,
            json!({"title": "winner", "referenceFrequency": 441.0}),
            "the rebase must carry BOTH edits, not overwrite the winner's"
        );
        assert_eq!(
            doc.sync_revision, 2,
            "the server's revision must be adopted"
        );
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Pending),
            "the rebased edit is still unsent"
        );
        assert_eq!(
            queued_update_rows(&db, &id).await,
            1,
            "the rebase must replace the queue row, not add to it"
        );
        assert_eq!(
            queued_base_hash(&db, &id).await.as_deref(),
            Some(server_hash.as_str()),
            "the resend must be locked to the server's current hash"
        );
        assert!(
            retry_rx.try_recv().is_ok(),
            "a successful rebase must trigger the resend"
        );
    }

    #[tokio::test]
    async fn a_patch_that_no_longer_applies_settles_as_a_conflict() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let base = base_content();
        let local = json!({"title": "base", "referenceFrequency": 441.0});
        seed_pending_edit(&db, id, &base, &local, 1).await;

        // The winner removed the very field this client edited, so the queued
        // `replace /referenceFrequency` can no longer apply.
        let server = json!({"title": "winner"});
        let events = dispatcher();
        let seen = event_probe(&events);
        let (upload_retry, mut retry_rx) = retry_probe();

        deliver_ack(
            &db,
            &events,
            &upload_retry,
            id,
            update_rejected(
                id,
                "hash_mismatch",
                Some((2, server.clone(), calculate_checksum(&server))),
            ),
        )
        .await;

        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(doc.content, server, "the server's copy becomes local truth");
        assert_eq!(doc.sync_revision, 2);
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Conflict)
        );
        assert_eq!(
            queued_update_rows(&db, &id).await,
            0,
            "an unresolvable patch must not stay queued"
        );
        assert!(
            retry_rx.try_recv().is_err(),
            "a conflict must not schedule a resend"
        );
        let emitted = drain_events(&events, &seen);
        assert!(
            emitted
                .iter()
                .any(|e| matches!(e, SyncEvent::ConflictDetected { .. })),
            "the host app must be told about the conflict: {:?}",
            emitted
        );
        assert!(
            emitted.iter().any(|e| matches!(
                e,
                SyncEvent::SyncError {
                    code: ReplicantErrorCode::UpdateConflict,
                    ..
                }
            )),
            "the conflict must carry a structured error code: {:?}",
            emitted
        );
    }

    /// A create rejected because the server already holds the document (a lost
    /// ack, or its content-hash dedupe). Resending the create would be refused
    /// again forever, so the client must adopt the server's copy — which
    /// consumes the create row — and resend the local edits as an update.
    #[tokio::test]
    async fn a_create_rejected_for_a_document_the_server_holds_becomes_an_update() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let created = json!({"title": "draft"});
        db.save_new_document_and_queue_create(&make_doc(id, created.clone(), 1))
            .await
            .unwrap();

        // Edit it while the create is still unsent — deliberately NOT via
        // `seed_pending_edit`, whose Synced seed would consume the create row
        // and leave nothing for this test to prove.
        let edited = json!({"title": "edited"});
        db.save_document_and_queue_patch(
            &make_doc(id, edited.clone(), 1),
            &create_patch(&created, &edited).unwrap(),
            replicant_core::protocol::ChangeEventType::Update,
            Some(&created),
        )
        .await
        .unwrap();
        assert!(
            db.has_queued_create(&id).await.unwrap(),
            "the create must still be owed going in, or this test proves nothing"
        );

        let server = json!({"title": "draft", "server": true});
        let (source, canned) = canned_source(vec![ServerMessage::GetDocumentResponse {
            id,
            content: server.clone(),
            sync_revision: 4,
            content_hash: calculate_checksum(&server),
            deleted: false,
        }]);
        let events = dispatcher();
        let (upload_retry, mut retry_rx) = retry_probe();

        deliver_ack_from(
            &db,
            &events,
            &upload_retry,
            id,
            create_rejected(id, "id_taken"),
            &source,
        )
        .await;

        assert_eq!(
            resync_attempts(&canned).await,
            vec![id],
            "a rejected create must ask the server what it actually holds"
        );
        assert!(
            !db.has_queued_create(&id).await.unwrap(),
            "adopting the server's copy must consume the create row"
        );
        assert!(
            matches!(
                classify_pending_upload(&db, &id).await,
                PendingUploadKind::Update { .. }
            ),
            "the next upload must be an update, not a second doomed create"
        );
        assert!(
            retry_rx.try_recv().is_ok(),
            "the rebased update must be resent"
        );
    }

    /// A create the server refused outright and does not hold. Resending cannot
    /// help, so the document is parked as a conflict — and NOT soft-deleted, the
    /// way a resync treats a missing document: this content never reached the
    /// server, so the local copy is the only one there is.
    #[tokio::test]
    async fn a_create_refused_outright_settles_as_a_conflict_without_losing_the_content() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let created = json!({"title": "draft"});
        db.save_new_document_and_queue_create(&make_doc(id, created.clone(), 1))
            .await
            .unwrap();

        let (source, canned) = canned_source(vec![ServerMessage::Error {
            code: ErrorCode::DocumentNotFound,
            message: "not found".to_string(),
        }]);
        let events = dispatcher();
        let seen = event_probe(&events);
        let (upload_retry, mut retry_rx) = retry_probe();

        deliver_ack_from(
            &db,
            &events,
            &upload_retry,
            id,
            create_rejected(id, "quota_exceeded"),
            &source,
        )
        .await;

        assert_eq!(resync_attempts(&canned).await, vec![id]);
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Conflict),
            "a refused create must leave the pending set so it stops being resent"
        );
        assert_eq!(
            db.get_document(&id).await.unwrap().content,
            created,
            "the only copy of this content must survive"
        );
        assert_eq!(
            deleted_at(&db, &id).await,
            None,
            "a refused create is not a deletion"
        );
        assert!(
            retry_rx.try_recv().is_err(),
            "a refused create must not schedule a resend"
        );

        let emitted = drain_events(&events, &seen);
        assert!(
            emitted
                .iter()
                .any(|e| matches!(e, SyncEvent::ConflictDetected { .. })),
            "the host app must be told: {:?}",
            emitted
        );
        assert!(
            emitted.iter().any(|e| matches!(
                e,
                SyncEvent::SyncError {
                    code: ReplicantErrorCode::UpdateConflict,
                    ..
                }
            )),
            "the refusal must carry a structured error code: {:?}",
            emitted
        );
    }

    /// The bound: a create that keeps being refused must not spin.
    #[tokio::test]
    async fn a_repeatedly_refused_create_recovers_only_once() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        db.save_new_document_and_queue_create(&make_doc(id, json!({"title": "draft"}), 1))
            .await
            .unwrap();

        let not_found = || ServerMessage::Error {
            code: ErrorCode::DocumentNotFound,
            message: "not found".to_string(),
        };
        let (source, canned) = canned_source(vec![not_found(), not_found()]);
        let events = dispatcher();
        let (upload_retry, _retry_rx) = retry_probe();

        for _ in 0..2 {
            deliver_ack_from(
                &db,
                &events,
                &upload_retry,
                id,
                create_rejected(id, "quota_exceeded"),
                &source,
            )
            .await;
        }

        assert_eq!(
            resync_attempts(&canned).await,
            vec![id],
            "the second refusal must not fetch again"
        );

        // The first refusal spends the budget and resolves: the fetch proved the
        // server does not hold the document, so it is parked. The second changes
        // nothing beyond the generic failure event.
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Conflict),
            "the resolved refusal stays parked, out of the pending set"
        );
        assert_eq!(
            db.get_document(&id).await.unwrap().content,
            json!({"title": "draft"}),
            "the local content must survive a refusal it cannot recover from"
        );
        assert!(
            db.has_queued_create(&id).await.unwrap(),
            "the create is still genuinely owed, so its row must stay"
        );
        assert_eq!(deleted_at(&db, &id).await, None);
    }

    /// The budget is per session and spent on the ATTEMPT, not on resolving it.
    /// A refusal that arrives while the client cannot reach the server therefore
    /// leaves the document pending with its create still owed, and no second try
    /// this session. That is the known limit: the next session gets a fresh
    /// budget, which is how a transient refusal eventually succeeds.
    #[tokio::test]
    async fn a_refusal_that_cannot_reach_the_server_leaves_the_document_for_next_session() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let created = json!({"title": "draft"});
        db.save_new_document_and_queue_create(&make_doc(id, created.clone(), 1))
            .await
            .unwrap();

        // An empty canned source records the attempt and answers like an
        // unreachable server.
        let (source, canned) = canned_source(vec![]);
        let events = dispatcher();
        let (upload_retry, _retry_rx) = retry_probe();

        for _ in 0..2 {
            deliver_ack_from(
                &db,
                &events,
                &upload_retry,
                id,
                create_rejected(id, "quota_exceeded"),
                &source,
            )
            .await;
        }

        assert_eq!(
            resync_attempts(&canned).await,
            vec![id],
            "the budget is spent on the attempt, so the second refusal does not try"
        );
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Pending),
            "an unresolved refusal stays pending so a later session retries it"
        );
        assert!(
            db.has_queued_create(&id).await.unwrap(),
            "the create is still owed"
        );
        assert_eq!(
            db.get_document(&id).await.unwrap().content,
            created,
            "the local content must survive"
        );
        assert_eq!(deleted_at(&db, &id).await, None);
    }

    #[tokio::test]
    async fn rebases_are_capped_then_the_document_conflicts() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let base = base_content();
        let local = json!({"title": "base", "referenceFrequency": 441.0});
        seed_pending_edit(&db, id, &base, &local, 1).await;

        // One shared UploadRetry: the attempt budget is per document per session.
        let (upload_retry, _retry_rx) = retry_probe();
        let events = dispatcher();

        for round in 1..=MAX_REBASE_ATTEMPTS {
            let server = json!({"title": format!("winner {}", round), "referenceFrequency": 440.0});
            deliver_ack(
                &db,
                &events,
                &upload_retry,
                id,
                update_rejected(
                    id,
                    "hash_mismatch",
                    Some((
                        1 + round as i64,
                        server.clone(),
                        calculate_checksum(&server),
                    )),
                ),
            )
            .await;
            assert_eq!(
                db.get_sync_status(&id).await.unwrap(),
                Some(SyncStatus::Pending),
                "round {} is within budget and must rebase",
                round
            );
        }

        let last = json!({"title": "winner last", "referenceFrequency": 440.0});
        deliver_ack(
            &db,
            &events,
            &upload_retry,
            id,
            update_rejected(
                id,
                "hash_mismatch",
                Some((99, last.clone(), calculate_checksum(&last))),
            ),
        )
        .await;

        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Conflict),
            "the budget must terminate the live-lock"
        );
        assert_eq!(doc.content, last, "the server's copy becomes local truth");
        assert_eq!(doc.sync_revision, 99);
    }

    /// Drive a document into a settled `Conflict` through the real path.
    async fn seed_settled_conflict(
        db: &Arc<ClientDatabase>,
        id: Uuid,
        server: &Value,
        revision: i64,
    ) {
        let base = base_content();
        // An edit to a field the winner removed cannot be rebased.
        let local = json!({"title": "base", "referenceFrequency": 441.0});
        seed_pending_edit(db, id, &base, &local, revision - 1).await;

        let (upload_retry, _rx) = retry_probe();
        deliver_ack(
            db,
            &dispatcher(),
            &upload_retry,
            id,
            update_rejected(
                id,
                "hash_mismatch",
                Some((revision, server.clone(), calculate_checksum(server))),
            ),
        )
        .await;

        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Conflict),
            "the fixture must actually reach Conflict"
        );
    }

    #[tokio::test]
    async fn a_broadcast_onto_a_settled_conflict_advances_content_and_keeps_the_flag() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let server = json!({"title": "winner"});
        seed_settled_conflict(&db, id, &server, 5).await;

        // The conflicted copy IS the server's content, so a contiguous patch
        // applies cleanly. What must NOT happen is the flag being cleared or
        // flipped to Pending, which would resend and lose the user's signal.
        let next = json!({"title": "winner", "note": "later"});
        let resynced = deliver(&db, &dispatcher(), server_patch(id, &server, &next, 6)).await;

        assert!(
            resynced.is_empty(),
            "a contiguous patch onto a conflict must apply, not resync"
        );
        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(doc.content, next, "content must keep following the server");
        assert_eq!(doc.sync_revision, 6);
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Conflict),
            "only a local edit may resolve a conflict"
        );
        assert_eq!(
            queued_update_rows(&db, &id).await,
            0,
            "nothing may be queued for upload"
        );
    }

    #[tokio::test]
    async fn a_resync_of_a_settled_conflict_keeps_the_flag() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let server = json!({"title": "winner"});
        seed_settled_conflict(&db, id, &server, 5).await;

        let fresh = json!({"title": "winner", "note": "much later"});
        Client::apply_resync_result(
            &db,
            Uuid::new_v4(),
            &dispatcher(),
            id,
            ServerMessage::GetDocumentResponse {
                id,
                content: fresh.clone(),
                sync_revision: 9,
                content_hash: calculate_checksum(&fresh),
                deleted: false,
            },
        )
        .await
        .unwrap();

        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(
            doc.content, fresh,
            "a conflict with nothing queued has no local edits to protect"
        );
        assert_eq!(doc.sync_revision, 9);
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Conflict)
        );
    }

    #[tokio::test]
    async fn a_local_edit_resolves_a_conflict_into_pending() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let server = json!({"title": "winner"});
        seed_settled_conflict(&db, id, &server, 5).await;

        // Editing the document is the user's implicit resolution.
        let resolved = json!({"title": "winner", "referenceFrequency": 441.0});
        let mut doc = db.get_document(&id).await.unwrap();
        let previous = doc.content.clone();
        doc.content = resolved.clone();
        db.save_document_and_queue_patch(
            &doc,
            &create_patch(&previous, &resolved).unwrap(),
            replicant_core::protocol::ChangeEventType::Update,
            Some(&previous),
        )
        .await
        .unwrap();

        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Pending),
            "a local edit must clear the conflict"
        );
        assert_eq!(queued_update_rows(&db, &id).await, 1);
    }

    /// Store content in a form `serde_json` would never emit. The row still
    /// parses to the same `Value`, so it reads as the pre-image, but the CAS
    /// binds the canonical text and can never match — a permanently losing
    /// swap, with no races involved.
    async fn wedge_the_swap(db: &Arc<ClientDatabase>, id: &Uuid, content: &Value) {
        sqlx::query("UPDATE documents SET content = ? WHERE id = ?")
            .bind(format!("{:#}", content))
            .bind(id.to_string())
            .execute(&db.pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_rebase_that_loses_the_swap_resends_and_spends_one_attempt() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let base = base_content();
        let local = json!({"title": "base", "referenceFrequency": 441.0});
        seed_pending_edit(&db, id, &base, &local, 1).await;
        wedge_the_swap(&db, &id, &local).await;
        let before = raw_content(&db, &id).await;

        let (upload_retry, mut retry_rx) = retry_probe();
        let server = json!({"title": "winner", "referenceFrequency": 440.0});
        deliver_ack(
            &db,
            &dispatcher(),
            &upload_retry,
            id,
            update_rejected(
                id,
                "hash_mismatch",
                Some((2, server.clone(), calculate_checksum(&server))),
            ),
        )
        .await;

        assert!(
            retry_rx.try_recv().is_ok(),
            "a lost swap must resend so the newer edit gets its own round"
        );
        assert!(
            retry_rx.try_recv().is_err(),
            "exactly one resend per lost round"
        );
        assert_eq!(
            raw_content(&db, &id).await,
            before,
            "a lost swap must write nothing"
        );
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Pending)
        );

        // Two more rounds spend the rest of the budget; the fourth has none
        // left and falls through to the conflict settlement.
        for _ in 0..2 {
            deliver_ack(
                &db,
                &dispatcher(),
                &upload_retry,
                id,
                update_rejected(
                    id,
                    "hash_mismatch",
                    Some((2, server.clone(), calculate_checksum(&server))),
                ),
            )
            .await;
            assert!(retry_rx.try_recv().is_ok(), "each round resends once");
        }

        deliver_ack(
            &db,
            &dispatcher(),
            &upload_retry,
            id,
            update_rejected(
                id,
                "hash_mismatch",
                Some((2, server.clone(), calculate_checksum(&server))),
            ),
        )
        .await;
        assert!(
            retry_rx.try_recv().is_err(),
            "the budget is spent, so no fourth resend"
        );
    }

    #[tokio::test]
    async fn a_conflict_settlement_that_loses_the_swap_leaves_the_document_alone() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let base = base_content();
        let local = json!({"title": "base", "referenceFrequency": 441.0});
        seed_pending_edit(&db, id, &base, &local, 1).await;
        wedge_the_swap(&db, &id, &local).await;
        let before = raw_content(&db, &id).await;

        // The winner removed the edited field, so the queued patch cannot
        // apply: this goes straight to settle_as_conflict, whose own swap is
        // wedged too.
        let server = json!({"title": "winner"});
        let (upload_retry, mut retry_rx) = retry_probe();
        deliver_ack(
            &db,
            &dispatcher(),
            &upload_retry,
            id,
            update_rejected(
                id,
                "hash_mismatch",
                Some((2, server.clone(), calculate_checksum(&server))),
            ),
        )
        .await;

        assert_eq!(
            raw_content(&db, &id).await,
            before,
            "a settlement that loses the swap must not clobber the newer edit"
        );
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Pending),
            "the document stays pending for its own upload round"
        );
        assert_eq!(
            queued_update_rows(&db, &id).await,
            1,
            "the queue row survives an uncommitted settlement"
        );
        assert!(
            retry_rx.try_recv().is_err(),
            "the settlement path terminates rather than resending"
        );
    }

    #[tokio::test]
    async fn a_rebase_that_changes_nothing_is_adopted_without_a_resend() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let base = base_content();
        let local = json!({"title": "base", "referenceFrequency": 441.0});
        seed_pending_edit(&db, id, &base, &local, 1).await;

        // The winner's update already carries this client's edit.
        let server = json!({"title": "winner", "referenceFrequency": 441.0});
        let (upload_retry, mut retry_rx) = retry_probe();
        deliver_ack(
            &db,
            &dispatcher(),
            &upload_retry,
            id,
            update_rejected(
                id,
                "hash_mismatch",
                Some((2, server.clone(), calculate_checksum(&server))),
            ),
        )
        .await;

        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(doc.content, server);
        assert_eq!(doc.sync_revision, 2);
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Synced),
            "nothing is outstanding once the edit is already on the server"
        );
        assert_eq!(queued_update_rows(&db, &id).await, 0);
        assert!(
            retry_rx.try_recv().is_err(),
            "a no-op patch must not cost an upload round trip"
        );
    }

    #[tokio::test]
    async fn an_adoption_that_loses_the_swap_keeps_the_queue_row() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let base = base_content();
        let local = json!({"title": "base", "referenceFrequency": 441.0});
        seed_pending_edit(&db, id, &base, &local, 1).await;
        wedge_the_swap(&db, &id, &local).await;
        let before = raw_content(&db, &id).await;

        // Server content already equals the local edit, so the rebase diff is
        // empty and this takes the adopt path — whose swap is wedged.
        let (upload_retry, mut retry_rx) = retry_probe();
        deliver_ack(
            &db,
            &dispatcher(),
            &upload_retry,
            id,
            update_rejected(
                id,
                "hash_mismatch",
                Some((2, local.clone(), calculate_checksum(&local))),
            ),
        )
        .await;

        assert_eq!(
            queued_update_rows(&db, &id).await,
            1,
            "clearing the queue must not outlive a failed swap: a Pending document \
             with no queue row would be re-sent as a create"
        );
        assert_eq!(raw_content(&db, &id).await, before);
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Pending)
        );
        assert!(
            retry_rx.try_recv().is_ok(),
            "a lost adoption falls back to resending"
        );
    }

    #[tokio::test]
    async fn clearing_the_queue_is_coupled_to_the_swap_winning() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let base = base_content();
        let local = json!({"title": "base", "referenceFrequency": 441.0});
        seed_pending_edit(&db, id, &base, &local, 1).await;

        let doc = db.get_document(&id).await.unwrap();
        let stale = DocumentPreImage {
            sync_revision: doc.sync_revision,
            content: json!({"title": "never stored"}),
            sync_status: SyncStatus::Pending,
        };

        assert!(
            !db.save_document_and_clear_queue_if_unchanged(&doc, SyncStatus::Synced, &stale)
                .await
                .unwrap(),
            "a stale pre-image must lose"
        );
        assert_eq!(
            queued_update_rows(&db, &id).await,
            1,
            "a rolled-back transaction must delete nothing"
        );
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Pending)
        );

        let current = DocumentPreImage {
            sync_revision: doc.sync_revision,
            content: doc.content.clone(),
            sync_status: SyncStatus::Pending,
        };
        assert!(db
            .save_document_and_clear_queue_if_unchanged(&doc, SyncStatus::Synced, &current)
            .await
            .unwrap());
        assert_eq!(
            queued_update_rows(&db, &id).await,
            0,
            "a winning swap clears the queue in the same transaction"
        );
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Synced)
        );
    }

    #[tokio::test]
    async fn a_rejection_without_server_state_falls_back_to_a_plain_retry() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let base = base_content();
        let local = json!({"title": "base", "referenceFrequency": 441.0});
        seed_pending_edit(&db, id, &base, &local, 1).await;

        let (upload_retry, mut retry_rx) = retry_probe();

        // A hash_mismatch whose current_* fields are missing cannot be rebased.
        deliver_ack(
            &db,
            &dispatcher(),
            &upload_retry,
            id,
            update_rejected(id, "hash_mismatch", None),
        )
        .await;

        assert!(
            retry_rx.try_recv().is_ok(),
            "without server state there is nothing to rebase, so retry blindly"
        );
        let doc = db.get_document(&id).await.unwrap();
        assert_eq!(doc.content, local, "local state must be left alone");
        assert_eq!(
            db.get_sync_status(&id).await.unwrap(),
            Some(SyncStatus::Pending)
        );
    }

    // ---------------------------------------------------------------------
    // Event origin (#44)
    // ---------------------------------------------------------------------

    /// A consumer that wants to react only to changes made elsewhere needs the
    /// event itself to say where the write came from; the payload alone cannot
    /// tell an echo of a local save from a genuine remote edit.
    #[tokio::test]
    async fn a_broadcast_applied_from_the_server_is_reported_as_remote() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let before = json!({"title": "A"});
        let after = json!({"title": "B"});
        seed(&db, &make_doc(id, before.clone(), 1), SyncStatus::Synced).await;

        let events = dispatcher();
        let seen = event_probe(&events);

        deliver(&db, &events, server_patch(id, &before, &after, 2)).await;

        let emitted = drain_events(&events, &seen);
        assert!(
            emitted.iter().any(|e| matches!(
                e,
                SyncEvent::DocumentUpdated {
                    origin: EventOrigin::Remote,
                    ..
                }
            )),
            "a patch applied from the server must be reported as Remote: {:?}",
            emitted
        );
    }

    /// A document deleted by another client arrives the same way and must also
    /// be reported as Remote.
    #[tokio::test]
    async fn a_delete_applied_from_the_server_is_reported_as_remote() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed(
            &db,
            &make_doc(id, json!({"title": "A"}), 1),
            SyncStatus::Synced,
        )
        .await;

        let events = dispatcher();
        let seen = event_probe(&events);

        Client::handle_server_message(
            ServerMessage::DocumentDeleted { document_id: id },
            &db,
            Uuid::new_v4(),
            &events,
            &recording_source().0,
        )
        .await
        .unwrap();

        let emitted = drain_events(&events, &seen);
        assert!(
            emitted.iter().any(|e| matches!(
                e,
                SyncEvent::DocumentDeleted {
                    origin: EventOrigin::Remote,
                    ..
                }
            )),
            "a delete applied from the server must be reported as Remote: {:?}",
            emitted
        );
    }
}

#[cfg(test)]
mod local_write_origin_tests {
    use super::*;
    use crate::events::{EventOrigin, SyncEvent};
    use serde_json::json;

    /// A client with no reachable server: every event it emits therefore
    /// originated from this process's own writes.
    async fn offline_client(events: Arc<EventDispatcher>) -> Client {
        Client::with_event_dispatcher(
            ":memory:",
            "ws://127.0.0.1:1",
            "origin-test@example.com",
            "key",
            "secret",
            None,
            Some(events),
        )
        .await
        .unwrap()
    }

    fn event_probe(events: &Arc<EventDispatcher>) -> Arc<std::sync::Mutex<Vec<SyncEvent>>> {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        events
            .register_rust_callback(move |event| sink.lock().unwrap().push(event))
            .unwrap();
        seen
    }

    fn drain_events(
        events: &Arc<EventDispatcher>,
        seen: &Arc<std::sync::Mutex<Vec<SyncEvent>>>,
    ) -> Vec<SyncEvent> {
        events.process_events().unwrap();
        seen.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn a_local_write_is_reported_as_local() {
        let events = Arc::new(EventDispatcher::new());
        let seen = event_probe(&events);
        let client = offline_client(events.clone()).await;

        let doc = client
            .create_document(json!({"title": "mine"}))
            .await
            .unwrap();
        client
            .update_document(doc.id, json!({"title": "mine, edited"}))
            .await
            .unwrap();

        let emitted = drain_events(&events, &seen);
        assert!(
            emitted.iter().any(|e| matches!(
                e,
                SyncEvent::DocumentCreated {
                    origin: EventOrigin::Local,
                    ..
                }
            )),
            "this client's own create must be reported as Local: {:?}",
            emitted
        );
        assert!(
            emitted.iter().any(|e| matches!(
                e,
                SyncEvent::DocumentUpdated {
                    origin: EventOrigin::Local,
                    ..
                }
            )),
            "this client's own update must be reported as Local: {:?}",
            emitted
        );
    }

    #[tokio::test]
    async fn a_local_delete_is_reported_as_local() {
        let events = Arc::new(EventDispatcher::new());
        let seen = event_probe(&events);
        let client = offline_client(events.clone()).await;

        let doc = client
            .create_document(json!({"title": "mine"}))
            .await
            .unwrap();
        client.delete_document(doc.id).await.unwrap();

        let emitted = drain_events(&events, &seen);
        assert!(
            emitted.iter().any(|e| matches!(
                e,
                SyncEvent::DocumentDeleted {
                    origin: EventOrigin::Local,
                    ..
                }
            )),
            "this client's own delete must be reported as Local: {:?}",
            emitted
        );
    }
}

#[cfg(test)]
mod start_and_shutdown_tests {
    use super::*;
    use crate::events::{EventType, SyncEvent};

    /// A client with credentials whose server refuses every connection.
    async fn offline_client(dir: &tempfile::TempDir) -> (Client, Arc<EventDispatcher>) {
        let db_url = format!(
            "sqlite:{}?mode=rwc",
            dir.path().join("client.sqlite3").display()
        );
        let dispatcher = Arc::new(EventDispatcher::new());
        let client = Client::open(
            &db_url,
            "ws://127.0.0.1:1/socket/websocket",
            "start-test@example.com",
            "rpa_test_key",
            "rps_test_secret",
            Some(Uuid::new_v4()),
            Some(dispatcher.clone()),
        )
        .await
        .unwrap();
        (client, dispatcher)
    }

    fn record_events(dispatcher: &EventDispatcher) -> Arc<std::sync::Mutex<Vec<EventType>>> {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        dispatcher
            .register_rust_callback(move |event: SyncEvent| {
                sink.lock().unwrap().push(event.event_type())
            })
            .unwrap();
        seen
    }

    #[tokio::test]
    async fn a_failed_initial_upload_leaves_a_working_client() {
        let dir = tempfile::tempdir().unwrap();
        let (client, dispatcher) = offline_client(&dir).await;
        let seen = record_events(&dispatcher);
        client
            .create_document(serde_json::json!({"title": "queued"}))
            .await
            .unwrap();

        // Connected but with no socket: the first pending upload fails.
        client.is_connected.store(true, Ordering::SeqCst);
        client.start().await;
        dispatcher.process_events().unwrap();

        assert!(!client.sync_protection_mode.load(Ordering::SeqCst));
        assert!(
            client.pending_uploads.lock().await.is_empty(),
            "a failed upload must not stay in flight"
        );
        let events = seen.lock().unwrap().clone();
        let started = events.iter().position(|e| *e == EventType::SyncStarted);
        let failed = events.iter().rposition(|e| *e == EventType::SyncError);
        assert!(
            started.is_some() && started < failed,
            "expected SyncStarted then SyncError: {:?}",
            events
        );
        assert!(!events.contains(&EventType::SyncCompleted));
        client
            .create_document(serde_json::json!({"title": "after start"}))
            .await
            .unwrap();

        client.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_stops_the_reconnection_monitor() {
        let dir = tempfile::tempdir().unwrap();
        let (client, dispatcher) = offline_client(&dir).await;
        let seen = record_events(&dispatcher);
        let attempts = |seen: &std::sync::Mutex<Vec<EventType>>| {
            dispatcher.process_events().unwrap();
            seen.lock()
                .unwrap()
                .iter()
                .filter(|e| **e == EventType::ConnectionAttempted)
                .count()
        };

        client.start().await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while attempts(&seen) == 0 {
            assert!(
                Instant::now() < deadline,
                "the monitor never tried to connect"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        client.shutdown().await;
        let at_shutdown = attempts(&seen);
        // The monitor would try again within its 5 s interval if still running.
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(attempts(&seen), at_shutdown);
        assert!(!client.is_connected());
    }
}
