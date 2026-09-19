use crate::queries::{DbHelpers, Queries};
use json_patch;
use replicant_core::patches::{calculate_checksum, create_patch};
use replicant_core::protocol::ChangeEventType;
use replicant_core::{
    models::{Document, SyncStatus},
    SyncError, SyncResult,
};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    Row, SqlitePool,
};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

/// Number of sqlx-sqlite worker threads this process has started for a
/// Replicant pool.
///
/// sqlx dedicates one OS thread to every sqlite connection and never hands out
/// a `JoinHandle` for it (`sqlx_sqlite::connection::worker`), so this counter
/// exists to make crash dumps and shutdown logs readable: it says how many
/// such threads Replicant is responsible for having started.
static SQLITE_WORKERS_STARTED: AtomicU64 = AtomicU64::new(0);

/// How many sqlx-sqlite worker threads Replicant has started in this process.
pub fn sqlite_workers_started() -> u64 {
    SQLITE_WORKERS_STARTED.load(Ordering::Relaxed)
}

/// How long sqlite waits on a lock another process holds before returning
/// SQLITE_BUSY. This is sqlx's own default, restated so the bound on a
/// contended connection setup is visible at the call site.
const SQLITE_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The state a caller verified before deciding to write. Passing it back to a
/// `*_if_unchanged` write turns read-check-write into a compare-and-swap, so a
/// local edit landing in between is never silently clobbered.
///
/// `content` compares as its stored JSON text: `serde_json` orders object keys
/// deterministically, so equal `Value`s always serialise to equal strings.
#[derive(Debug, Clone)]
pub struct DocumentPreImage {
    pub sync_revision: i64,
    pub content: serde_json::Value,
    pub sync_status: SyncStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingDocumentInfo {
    pub id: Uuid,
    pub is_deleted: bool,
}

pub struct ClientDatabase {
    pub pool: SqlitePool,
}

/// Dropping a pool without closing it is the DEV-1118 bug in miniature.
///
/// NOTHING JOINS sqlx's per-connection worker threads — sqlx spawns them
/// detached and keeps no `JoinHandle`. Awaiting `Pool::close()` is as close as
/// the API gets: it resolves only once the pool's size reaches zero, so every
/// connection, including ones still inside `establish()` or mid-statement, has
/// finished and had `sqlite3_close` called on it. What remains after that is the
/// worker's own thread epilogue, which is libstd code linked into this module
/// and runs for a few microseconds more. Closing shrinks the window; it does not
/// remove it, which is why a module that can be unloaded must still be pinned
/// (see `replicant_destroy`).
///
/// A plain drop is much worse than that: it ends an *idle* worker (the command
/// channel closes and its loop exits) but tells a busy one nothing, so a worker
/// blocked on SQLITE_BUSY keeps running here for as long as sqlite takes.
///
/// Nothing in this impl can close the pool — `close()` is async and `Drop` is
/// not — so it only says so, loudly, for the next person to find in a log.
impl Drop for ClientDatabase {
    fn drop(&mut self) {
        if !self.pool.is_closed() {
            tracing::warn!(
                "DB: a ClientDatabase was dropped without close(); a busy sqlite \
                 worker thread can outlive it and keep executing this module"
            );
        }
    }
}

impl ClientDatabase {
    pub async fn new(database_url: &str) -> SyncResult<Self> {
        // Same URL parsing `SqlitePoolOptions::connect` would do; spelled out so
        // the connect options can be named:
        //
        //  * `thread_name` labels sqlx's per-connection worker threads as ours,
        //    which is how DEV-1118 was identified in a minidump, and counts
        //    them so shutdown can report how many we started.
        //  * `busy_timeout` is sqlx's own default, restated here because it is
        //    the bound on how long a contended `establish()` can keep a worker
        //    thread alive inside this module (see `ffi::Replicant::shutdown`).
        let connect_options = SqliteConnectOptions::from_str(database_url)?
            .busy_timeout(SQLITE_BUSY_TIMEOUT)
            .thread_name(|id| {
                SQLITE_WORKERS_STARTED.fetch_add(1, Ordering::Relaxed);
                format!("replicant-sqlite-{id}")
            });

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(connect_options)
            .await?;

        Ok(Self { pool })
    }

    /// Closes the pool, awaiting every connection's shutdown.
    ///
    /// `Pool::close()` only resolves once the pool's size reaches zero, which
    /// covers connections still inside `establish()` as well as live ones, so
    /// awaiting it is what stops a sqlite worker thread from running on past
    /// the handle that started it. Idempotent.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    pub async fn run_migrations(&self) -> SyncResult<()> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }

    pub async fn ensure_user_config(&self, server_url: &str) -> SyncResult<()> {
        // Check if user_config already exists
        let exists = sqlx::query("SELECT COUNT(*) as count FROM user_config")
            .fetch_one(&self.pool)
            .await?;

        let count: i64 = exists.try_get("count")?;

        if count == 0 {
            // No user config exists, create default
            let user_id = Uuid::new_v4();
            let client_id = Uuid::new_v4();

            sqlx::query(
                "INSERT INTO user_config (user_id, client_id, server_url) VALUES (?1, ?2, ?3)",
            )
            .bind(user_id.to_string())
            .bind(client_id.to_string())
            .bind(server_url)
            .execute(&self.pool)
            .await?;
        }

        Ok(())
    }

    pub async fn ensure_user_config_with_identifier(
        &self,
        server_url: &str,
        _user_identifier: &str,
    ) -> SyncResult<()> {
        // Check if user_config already exists
        let exists = sqlx::query("SELECT COUNT(*) as count FROM user_config")
            .fetch_one(&self.pool)
            .await?;

        let count: i64 = exists.try_get("count")?;

        if count == 0 {
            // Provisional random id; the server's canonical id is adopted on first contact.
            let user_id = Uuid::new_v4();
            let client_id = Uuid::new_v4(); // Client ID should always be unique per instance

            sqlx::query(
                "INSERT INTO user_config (user_id, client_id, server_url, identity_adopted) VALUES (?1, ?2, ?3, 0)",
            )
            .bind(user_id.to_string())
            .bind(client_id.to_string())
            .bind(server_url)
            .execute(&self.pool)
            .await?;
        }

        Ok(())
    }

    pub async fn get_user_id(&self) -> SyncResult<Uuid> {
        let row = sqlx::query(Queries::GET_USER_ID)
            .fetch_one(&self.pool)
            .await?;

        let user_id: String = row.try_get("user_id")?;
        Ok(Uuid::parse_str(&user_id)?)
    }

    pub async fn is_identity_adopted(&self) -> SyncResult<bool> {
        let row = sqlx::query("SELECT identity_adopted FROM user_config LIMIT 1")
            .fetch_one(&self.pool)
            .await?;
        let adopted: i64 = row.try_get("identity_adopted")?;
        Ok(adopted != 0)
    }

    pub async fn get_client_id(&self) -> SyncResult<Uuid> {
        let row = sqlx::query(Queries::GET_CLIENT_ID)
            .fetch_one(&self.pool)
            .await?;

        let client_id: String = row.try_get("client_id")?;
        Ok(Uuid::parse_str(&client_id)?)
    }

    pub async fn get_user_and_client_id(&self) -> SyncResult<(Uuid, Uuid)> {
        let row = sqlx::query(Queries::GET_USER_AND_CLIENT_ID)
            .fetch_one(&self.pool)
            .await?;

        let user_id: String = row.try_get("user_id")?;
        let client_id: String = row.try_get("client_id")?;
        Ok((Uuid::parse_str(&user_id)?, Uuid::parse_str(&client_id)?))
    }

    /// Atomically adopt the server's canonical id: re-stamp local documents
    /// owned by `old_id` and flip `user_config` to `canonical_id` with
    /// `identity_adopted = 1`. A crash mid-adoption leaves the old id intact.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::InvalidOperation`] if `canonical_id` is nil, equals
    /// `old_id`, the identity was already adopted, or no `user_config` row
    /// matches `old_id`; database failures surface as the underlying error.
    pub async fn adopt_identity(&self, old_id: Uuid, canonical_id: Uuid) -> SyncResult<()> {
        if canonical_id.is_nil() || old_id == canonical_id {
            return Err(SyncError::InvalidOperation(
                "adopt_identity: canonical_id must be non-nil and differ from old_id".to_string(),
            ));
        }
        if self.is_identity_adopted().await? {
            return Err(SyncError::InvalidOperation(
                "adopt_identity: identity has already been adopted".to_string(),
            ));
        }

        let mut tx = self.pool.begin().await?;

        sqlx::query(Queries::RESTAMP_DOCUMENTS_USER_ID)
            .bind(canonical_id.to_string())
            .bind(old_id.to_string())
            .execute(&mut *tx)
            .await?;

        let result = sqlx::query(Queries::ADOPT_USER_CONFIG_IDENTITY)
            .bind(canonical_id.to_string())
            .bind(old_id.to_string())
            .execute(&mut *tx)
            .await?;

        if result.rows_affected() != 1 {
            return Err(SyncError::InvalidOperation(format!(
                "adopt_identity: expected to update 1 user_config row for old_id {}, got {}",
                old_id,
                result.rows_affected()
            )));
        }

        tx.commit().await?;
        Ok(())
    }

    pub async fn get_document(&self, id: &Uuid) -> SyncResult<Document> {
        let row = sqlx::query(Queries::GET_DOCUMENT)
            .bind(id.to_string())
            .fetch_one(&self.pool)
            .await?;

        DbHelpers::parse_document(&row)
    }

    /// Read the row's `sync_status`. `Ok(None)` means the document is not in
    /// the local database. `Document` does not carry the status, but the
    /// broadcast guard has to know whether local edits are outstanding.
    ///
    /// An unparseable status fails CLOSED as `Pending`. Both `Synced` and
    /// `Conflict` authorise a guarded patch apply — a settled conflict holds the
    /// server's content, so applying to it is safe — and neither may be reached
    /// by way of a fallback: a drifted row might be hiding unsent local edits,
    /// and the compare-and-swap cannot tell, because its content would match.
    /// `Pending` is the only status that is both non-appliable and
    /// non-destructive: it routes the document to a resync, which rebases the
    /// local content forward rather than overwriting it.
    pub async fn get_sync_status(&self, id: &Uuid) -> SyncResult<Option<SyncStatus>> {
        let row = sqlx::query("SELECT sync_status FROM documents WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;

        match row {
            Some(row) => {
                let raw: String = row.try_get("sync_status")?;
                Ok(Some(raw.parse::<SyncStatus>().unwrap_or_else(|_| {
                    tracing::warn!(
                        "DATABASE: Unrecognised sync_status {:?} for {}, treating as pending",
                        raw,
                        id
                    );
                    SyncStatus::Pending
                })))
            }
            None => Ok(None),
        }
    }

    /// Overwrite a document only if it still matches `expected`. Returns
    /// `false` when a concurrent local write landed first, in which case the
    /// caller must back off instead of overwriting it.
    ///
    /// Attribution columns are deliberately left untouched: this write carries
    /// sync state, not authorship.
    pub async fn save_document_if_unchanged(
        &self,
        doc: &Document,
        new_status: SyncStatus,
        expected: &DocumentPreImage,
    ) -> SyncResult<bool> {
        let params = DbHelpers::document_to_params(doc, Some(new_status))?;

        let result = sqlx::query(
            "UPDATE documents SET content = ?, sync_revision = ?, updated_at = ?, sync_status = ?, title = ? \
             WHERE id = ? AND content = ? AND sync_revision = ? AND sync_status = ?",
        )
        .bind(params.2) // content
        .bind(params.3) // sync_revision
        .bind(params.5) // updated_at
        .bind(params.7) // sync_status
        .bind(params.8) // title
        .bind(doc.id.to_string())
        .bind(serde_json::to_string(&expected.content)?)
        .bind(expected.sync_revision)
        .bind(expected.sync_status.to_string())
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            tracing::warn!(
                "DATABASE: Compare-and-swap for {} lost to a concurrent local write",
                doc.id
            );
            return Ok(false);
        }

        if let Err(e) = self.update_fts_for_document(&doc.id).await {
            tracing::warn!("FTS: Failed to update index for {}: {:?}", doc.id, e);
        }

        Ok(true)
    }

    /// Adopt server state and clear the document's sync queue, in one
    /// transaction.
    ///
    /// Doing these as two statements leaves a window: a local edit landing
    /// between them sets `Pending` and inserts a fresh queue row, which the
    /// unscoped delete then removes — leaving a `Pending` document with nothing
    /// queued, which the next upload pass sends as a `CreateDocument` for a
    /// document the server already has.
    ///
    /// Returns `false` if the row no longer matches `expected`, in which case
    /// nothing is deleted.
    pub async fn save_document_and_clear_queue_if_unchanged(
        &self,
        doc: &Document,
        new_status: SyncStatus,
        expected: &DocumentPreImage,
    ) -> SyncResult<bool> {
        let params = DbHelpers::document_to_params(doc, Some(new_status))?;
        let mut tx = self.pool.begin().await?;

        let result = sqlx::query(
            "UPDATE documents SET content = ?, sync_revision = ?, updated_at = ?, sync_status = ?, title = ? \
             WHERE id = ? AND content = ? AND sync_revision = ? AND sync_status = ?",
        )
        .bind(params.2) // content
        .bind(params.3) // sync_revision
        .bind(params.5) // updated_at
        .bind(params.7) // sync_status
        .bind(params.8) // title
        .bind(doc.id.to_string())
        .bind(serde_json::to_string(&expected.content)?)
        .bind(expected.sync_revision)
        .bind(expected.sync_status.to_string())
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() == 0 {
            tx.rollback().await?;
            tracing::warn!(
                "DATABASE: Compare-and-swap for {} lost to a concurrent local write",
                doc.id
            );
            return Ok(false);
        }

        sqlx::query("DELETE FROM sync_queue WHERE document_id = ?")
            .bind(doc.id.to_string())
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        if let Err(e) = self.update_fts_for_document(&doc.id).await {
            tracing::warn!("FTS: Failed to update index for {}: {:?}", doc.id, e);
        }

        Ok(true)
    }

    /// Rebase a document carrying unsent local edits onto a fresh server base:
    /// adopt `new_content` at the server's revision and replace the queued
    /// update patch, in one transaction.
    ///
    /// `new_content` is what the local copy should now show. A resync passes the
    /// content it already has (the user's unsent edit stays visible); a
    /// `hash_mismatch` rebase passes the merged result of replaying the queued
    /// patch onto the server's content.
    ///
    /// `base_content` is the server content being rebased onto, and
    /// `base_content_hash` its hash. The queued patch is the diff between them,
    /// so it is derived here rather than passed in — the same base the row
    /// stores for later regeneration by [`Self::get_queued_patch`].
    ///
    /// Prior `update` rows for the document are deleted rather than added to.
    /// `get_queued_patch` reads exactly one row, so a leftover row with an
    /// outdated base hash would be retried — and rejected — forever. Any
    /// `create` row goes too: adopting server content is the create's ack.
    ///
    /// `new_status` is what the row should carry afterwards: `Pending` for a
    /// rebase whose patch is about to be sent, or the document's existing status
    /// where that must survive (an unresolved `Conflict` is not cleared by
    /// anything but a local edit).
    ///
    /// Returns `false` if the row no longer matches `expected`.
    #[allow(clippy::too_many_arguments)]
    pub async fn rebase_pending_document_if_unchanged(
        &self,
        document_id: &Uuid,
        new_content: &serde_json::Value,
        new_sync_revision: i64,
        base_content: &serde_json::Value,
        base_content_hash: &str,
        new_status: SyncStatus,
        expected: &DocumentPreImage,
    ) -> SyncResult<bool> {
        let mut tx = self.pool.begin().await?;

        let result = sqlx::query(
            "UPDATE documents SET content = ?, sync_revision = ?, updated_at = ?, sync_status = ? \
             WHERE id = ? AND content = ? AND sync_revision = ? AND sync_status = ?",
        )
        .bind(serde_json::to_string(new_content)?)
        .bind(new_sync_revision)
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(new_status.to_string())
        .bind(document_id.to_string())
        .bind(serde_json::to_string(&expected.content)?)
        .bind(expected.sync_revision)
        .bind(expected.sync_status.to_string())
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() == 0 {
            tx.rollback().await?;
            tracing::warn!(
                "DATABASE: Rebase of {} lost to a concurrent local write",
                document_id
            );
            return Ok(false);
        }

        sqlx::query("DELETE FROM sync_queue WHERE document_id = ? AND operation_type = ?")
            .bind(document_id.to_string())
            .bind(ChangeEventType::Update.to_string())
            .execute(&mut *tx)
            .await?;

        // Rebasing onto server content proves the server holds this document,
        // which is exactly what an unconsumed `create` row denies. Leaving it
        // would make the next upload send a create the server answers with a
        // primary-key conflict, and nothing clears a rejected create.
        sqlx::query("DELETE FROM sync_queue WHERE document_id = ? AND operation_type = ?")
            .bind(document_id.to_string())
            .bind(ChangeEventType::Create.to_string())
            .execute(&mut *tx)
            .await?;

        sqlx::query(
            "INSERT INTO sync_queue (document_id, operation_type, patch, old_content_hash, base_content) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(document_id.to_string())
        .bind(ChangeEventType::Update.to_string())
        .bind(serde_json::to_string(&create_patch(base_content, new_content)?)?)
        .bind(base_content_hash)
        .bind(serde_json::to_string(base_content)?)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        if let Err(e) = self.update_fts_for_document(document_id).await {
            tracing::warn!("FTS: Failed to update index for {}: {:?}", document_id, e);
        }

        Ok(true)
    }

    pub async fn save_document(&self, doc: &Document) -> SyncResult<()> {
        self.save_document_with_status(doc, None).await
    }

    pub(crate) async fn save_document_with_status(
        &self,
        doc: &Document,
        sync_status: Option<SyncStatus>,
    ) -> SyncResult<()> {
        let status_str = sync_status
            .as_ref()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "synced".to_string());
        tracing::info!(
            "DATABASE: 💾 Saving document {} with status: {}, sync_revision: {}",
            doc.id,
            status_str,
            doc.sync_revision
        );

        let adopts_server_state = matches!(sync_status, Some(SyncStatus::Synced));
        let params = DbHelpers::document_to_params(doc, sync_status)?;

        // One transaction: a crash between the write and the queue clear would
        // leave a Synced document still owing a create, which is exactly the
        // invariant violation the create row exists to prevent.
        let mut tx = self.pool.begin().await?;

        sqlx::query(Queries::UPSERT_DOCUMENT)
            .bind(params.0) // id
            .bind(params.1) // user_id
            .bind(params.2) // content
            .bind(params.3) // version
            .bind(params.4) // created_at
            .bind(params.5) // updated_at
            .bind(params.6) // deleted_at
            .bind(params.7) // sync_status
            .bind(params.8) // title
            .bind(params.9) // author_name
            .bind(params.10) // visibility
            .bind(params.11) // provenance
            .execute(&mut *tx)
            .await?;

        // Writing a document as Synced means the server holds it, so it no
        // longer owes a create. The broadcast and full-sync paths adopt server
        // state this way; a `create` row left behind here would make a later
        // local edit upload as a create the server rejects as a duplicate.
        if adopts_server_state {
            sqlx::query("DELETE FROM sync_queue WHERE document_id = ? AND operation_type = ?")
                .bind(doc.id.to_string())
                .bind(ChangeEventType::Create.to_string())
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;

        tracing::info!("DATABASE: ✅ Document {} saved successfully", doc.id);

        // Update FTS index for this document
        if let Err(e) = self.update_fts_for_document(&doc.id).await {
            tracing::warn!("FTS: Failed to update index for {}: {:?}", doc.id, e);
        }

        Ok(())
    }

    pub async fn get_pending_documents(&self) -> SyncResult<Vec<PendingDocumentInfo>> {
        tracing::info!("DATABASE: 🔍 Querying for pending documents...");
        let rows = sqlx::query(Queries::GET_PENDING_DOCUMENTS)
            .bind(SyncStatus::Pending.to_string())
            .fetch_all(&self.pool)
            .await?;

        tracing::info!("DATABASE: Found {} pending documents", rows.len());

        let mut pending_docs = Vec::new();
        for row in rows {
            let id: String = row.try_get("id")?;
            let deleted_at: Option<String> = row.try_get("deleted_at")?;

            let doc_info = PendingDocumentInfo {
                id: Uuid::parse_str(&id)?,
                is_deleted: deleted_at.is_some(),
            };

            tracing::info!(
                "DATABASE: Pending doc: {} | Deleted: {}",
                doc_info.id,
                doc_info.is_deleted
            );

            pending_docs.push(doc_info);
        }

        Ok(pending_docs)
    }

    /// Settle a document against the server.
    ///
    /// Every caller is an ack, an adopt, or a local soft-delete that matches the
    /// server — contexts in which the document, one way or another, no longer
    /// owes the server a create. So this consumes any `create` row: leaving one
    /// would make the next local edit upload as a create the server rejects as a
    /// duplicate.
    pub async fn mark_synced(&self, document_id: &Uuid) -> SyncResult<()> {
        tracing::info!("DATABASE: 🔄 Marking document {} as synced", document_id);

        // One transaction, for the same reason as `save_document_with_status`:
        // a crash between the two would leave a Synced document still owing a
        // create.
        let mut tx = self.pool.begin().await?;

        let result = sqlx::query(Queries::MARK_DOCUMENT_SYNCED)
            .bind(SyncStatus::Synced.to_string())
            .bind(document_id.to_string())
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM sync_queue WHERE document_id = ? AND operation_type = ?")
            .bind(document_id.to_string())
            .bind(ChangeEventType::Create.to_string())
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        tracing::info!(
            "DATABASE: ✅ Marked {} as synced, rows affected: {}",
            document_id,
            result.rows_affected()
        );

        Ok(())
    }

    pub async fn update_sync_revision(
        &self,
        document_id: &Uuid,
        sync_revision: i64,
    ) -> SyncResult<()> {
        tracing::info!(
            "DATABASE: 🔄 Updating document {} sync_revision to {}",
            document_id,
            sync_revision
        );

        let result = sqlx::query("UPDATE documents SET sync_revision = ? WHERE id = ?")
            .bind(sync_revision)
            .bind(document_id.to_string())
            .execute(&self.pool)
            .await?;

        tracing::info!(
            "DATABASE: ✅ Updated {} sync_revision, rows affected: {}",
            document_id,
            result.rows_affected()
        );

        Ok(())
    }

    pub async fn update_attribution(
        &self,
        document_id: &Uuid,
        author_name: Option<String>,
        visibility: Option<String>,
        provenance: Option<serde_json::Value>,
    ) -> SyncResult<()> {
        tracing::info!("DATABASE: 🔄 Updating document {} attribution", document_id);

        let provenance_str = provenance.map(|v| v.to_string());

        let result = sqlx::query(
            "UPDATE documents SET author_name = ?, visibility = ?, provenance = ? WHERE id = ?",
        )
        .bind(author_name)
        .bind(visibility)
        .bind(provenance_str)
        .bind(document_id.to_string())
        .execute(&self.pool)
        .await?;

        tracing::info!(
            "DATABASE: ✅ Updated {} attribution, rows affected: {}",
            document_id,
            result.rows_affected()
        );

        Ok(())
    }

    pub async fn delete_document(&self, document_id: &Uuid) -> SyncResult<()> {
        sqlx::query("UPDATE documents SET deleted_at = ?, sync_status = ? WHERE id = ?")
            .bind(chrono::Utc::now())
            .bind(SyncStatus::Pending.to_string())
            .bind(document_id.to_string())
            .execute(&self.pool)
            .await?;

        // Remove from FTS index
        if let Err(e) = self.update_fts_for_document(document_id).await {
            tracing::warn!("FTS: Failed to remove {} from index: {:?}", document_id, e);
        }

        Ok(())
    }

    pub async fn get_all_documents(&self) -> SyncResult<Vec<Document>> {
        let rows = sqlx::query("SELECT * FROM documents WHERE deleted_at IS NULL")
            .fetch_all(&self.pool)
            .await?;

        rows.into_iter()
            .map(|row| DbHelpers::parse_document(&row))
            .collect()
    }

    pub async fn get_all_document_ids(&self, include_deleted: bool) -> SyncResult<Vec<Uuid>> {
        let query = if include_deleted {
            "SELECT id FROM documents"
        } else {
            "SELECT id FROM documents WHERE deleted_at IS NULL"
        };

        let rows = sqlx::query(query).fetch_all(&self.pool).await?;

        rows.into_iter()
            .map(|row| {
                let id: String = row.get("id");
                Ok(Uuid::parse_str(&id)?)
            })
            .collect()
    }

    pub async fn count_documents(&self) -> SyncResult<i64> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM documents WHERE deleted_at IS NULL")
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }

    pub async fn queue_sync_operation(
        &self,
        document_id: &Uuid,
        operation_type: ChangeEventType,
        patch: Option<&json_patch::Patch>,
    ) -> SyncResult<()> {
        let patch_json = patch.map(serde_json::to_string).transpose()?;

        tracing::info!(
            "DATABASE: queue_sync_operation called: doc_id={}, op_type={}, patch_size={}",
            document_id,
            operation_type.to_string(),
            patch_json.as_ref().map(|p| p.len()).unwrap_or(0)
        );

        let result = sqlx::query(Queries::INSERT_SYNC_QUEUE)
            .bind(document_id.to_string())
            .bind(operation_type.to_string())
            .bind(patch_json.clone())
            .execute(&self.pool)
            .await?;

        tracing::info!(
            "DATABASE: sync_queue insert successful: rows_affected={}, doc_id={}",
            result.rows_affected(),
            document_id
        );

        // Verify the insert by immediately querying
        let count_result =
            sqlx::query("SELECT COUNT(*) as count FROM sync_queue WHERE document_id = ?")
                .bind(document_id.to_string())
                .fetch_one(&self.pool)
                .await;

        match count_result {
            Ok(row) => {
                let count: i64 = row.try_get("count").unwrap_or(0);
                tracing::info!(
                    "DATABASE: sync_queue verification: {} entries for doc_id={}",
                    count,
                    document_id
                );
            }
            Err(e) => {
                tracing::error!("DATABASE: Failed to verify sync_queue insert: {}", e);
            }
        }

        Ok(())
    }

    /// Save a newly created document and queue its `create` in one transaction,
    /// so a crash between the two can never leave a document the server will
    /// never be told about.
    ///
    /// The queued row is the durable record that this document has NOT reached
    /// the server. Local edits made before it is sent queue their own `update`
    /// row beside it and leave it alone; [`Self::has_queued_create`] is what the
    /// upload path reads to know it must send a create, and the create it sends
    /// carries the document's current content, which already folds those edits
    /// in.
    ///
    /// The row carries no patch: a create uploads the whole document.
    pub async fn save_new_document_and_queue_create(&self, doc: &Document) -> SyncResult<()> {
        let params = DbHelpers::document_to_params(doc, Some(SyncStatus::Pending))?;
        let mut tx = self.pool.begin().await?;

        sqlx::query(Queries::UPSERT_DOCUMENT)
            .bind(params.0) // id
            .bind(params.1) // user_id
            .bind(params.2) // content
            .bind(params.3) // version
            .bind(params.4) // created_at
            .bind(params.5) // updated_at
            .bind(params.6) // deleted_at
            .bind(params.7) // sync_status
            .bind(params.8) // title
            .bind(params.9) // author_name
            .bind(params.10) // visibility
            .bind(params.11) // provenance
            .execute(&mut *tx)
            .await?;

        sqlx::query(Queries::INSERT_SYNC_QUEUE)
            .bind(doc.id.to_string())
            .bind(ChangeEventType::Create.to_string())
            .bind(Option::<String>::None)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        tracing::info!(
            "DATABASE: Atomically saved new document {} with pending status and queued its create",
            doc.id
        );

        if let Err(e) = self.update_fts_for_document(&doc.id).await {
            tracing::warn!("FTS: Failed to update index for {}: {:?}", doc.id, e);
        }

        Ok(())
    }

    /// Whether the document still carries an unconsumed `create` row — it was
    /// created locally and the server has never acknowledged it.
    ///
    /// An ack clears every row for the document, so this goes false exactly when
    /// the create lands. Until then it outranks any queued update: the server
    /// cannot patch a document it does not hold, and answers `not_found`.
    pub async fn has_queued_create(&self, document_id: &Uuid) -> SyncResult<bool> {
        let row = sqlx::query(
            "SELECT 1 FROM sync_queue WHERE document_id = ? AND operation_type = 'create' LIMIT 1",
        )
        .bind(document_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.is_some())
    }

    /// Atomically save a document and queue the edit for upload, so a crash
    /// between the two can never lose the edit.
    ///
    /// `base_content` is the content this edit was made against. A document
    /// keeps exactly ONE queued `update` row, whose base is the last state the
    /// server acknowledged: the first pending edit stores `base_content`, and
    /// every later edit made before that row is sent keeps the base it already
    /// has. Appending a row per edit instead would send the newest fragment
    /// paired with a base the server never had, which the server rejects as a
    /// `hash_mismatch`.
    ///
    /// The base only moves when the row is cleared (an ack) or rebased onto
    /// fresh server content; see [`Self::rebase_pending_document_if_unchanged`].
    ///
    /// `patch` is stored as given only for rows with no base (`create` and
    /// `delete`). For an update the stored patch is derived from the retained
    /// base and `doc.content`, which is what [`Self::get_queued_patch`] serves.
    ///
    /// A pre-upgrade row with no `base_content` cannot be extended — its base is
    /// unrecoverable — so it is replaced by a row based on this edit. The
    /// earlier offline edit then resolves through the `hash_mismatch` rebase.
    pub async fn save_document_and_queue_patch(
        &self,
        doc: &Document,
        patch: &json_patch::Patch,
        operation_type: ChangeEventType,
        base_content: Option<&serde_json::Value>,
    ) -> SyncResult<()> {
        // Start a transaction for atomicity
        let mut tx = self.pool.begin().await?;

        // Save document with pending status (in transaction)
        let params = DbHelpers::document_to_params(doc, Some(SyncStatus::Pending))?;

        sqlx::query(Queries::UPSERT_DOCUMENT)
            .bind(params.0) // id
            .bind(params.1) // user_id
            .bind(params.2) // content
            .bind(params.3) // version
            .bind(params.4) // created_at
            .bind(params.5) // updated_at
            .bind(params.6) // deleted_at
            .bind(params.7) // sync_status
            .bind(params.8) // title
            .execute(&mut *tx)
            .await?;

        // Queue sync operation (in transaction)
        if let Some(edit_base) = base_content {
            // An update: collapse onto the single row, keeping whatever base it
            // already carries. Oldest-first is the earliest base, which only
            // several pre-upgrade rows can offer — this build keeps exactly one,
            // so the reader's newest-first order picks the same row.
            let existing = sqlx::query(
                "SELECT base_content, old_content_hash FROM sync_queue \
                 WHERE document_id = ? AND operation_type = ? ORDER BY id ASC LIMIT 1",
            )
            .bind(doc.id.to_string())
            .bind(operation_type.to_string())
            .fetch_optional(&mut *tx)
            .await?;

            let retained = match existing {
                Some(row) => {
                    let base_json: Option<String> = row.try_get("base_content")?;
                    let hash: Option<String> = row.try_get("old_content_hash")?;
                    base_json
                        .map(|json| serde_json::from_str::<serde_json::Value>(&json))
                        .transpose()?
                        .map(|base| {
                            let hash = hash.unwrap_or_else(|| calculate_checksum(&base));
                            (base, hash)
                        })
                }
                None => None,
            };

            let (base, base_hash) =
                retained.unwrap_or_else(|| (edit_base.clone(), calculate_checksum(edit_base)));

            sqlx::query("DELETE FROM sync_queue WHERE document_id = ? AND operation_type = ?")
                .bind(doc.id.to_string())
                .bind(operation_type.to_string())
                .execute(&mut *tx)
                .await?;

            sqlx::query(
                "INSERT INTO sync_queue (document_id, operation_type, patch, old_content_hash, base_content) VALUES (?, ?, ?, ?, ?)"
            )
            .bind(doc.id.to_string())
            .bind(operation_type.to_string())
            .bind(serde_json::to_string(&create_patch(&base, &doc.content)?)?)
            .bind(base_hash)
            .bind(serde_json::to_string(&base)?)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(Queries::INSERT_SYNC_QUEUE)
                .bind(doc.id.to_string()) // document_id
                .bind(operation_type.to_string()) // operation_type
                .bind(serde_json::to_string(patch)?) // patch
                .execute(&mut *tx)
                .await?;
        }

        // Commit atomically - both operations succeed or both fail
        tx.commit().await?;

        tracing::info!(
            "DATABASE: Atomically saved document {} with pending status and queued patch",
            doc.id
        );

        // Update FTS index for this document
        if let Err(e) = self.update_fts_for_document(&doc.id).await {
            tracing::warn!("FTS: Failed to update index for {}: {:?}", doc.id, e);
        }

        Ok(())
    }

    /// The document's queued update as one cumulative diff, with the hash of the
    /// base it applies to.
    ///
    /// The patch is regenerated here from the row's stored `base_content` and
    /// the document's CURRENT content, so however many local edits accumulated
    /// since the base was set, the upload is a single diff the server can apply
    /// to the state it actually holds.
    ///
    /// `Ok(None)` means nothing is queued — the caller's signal that a pending
    /// document is a create, not an update.
    ///
    /// A row written before `base_content` existed falls back to its stored
    /// patch and hash, which is what a pre-upgrade client would have sent. A NULL
    /// base is that legacy case; a column that fails to read is corruption and is
    /// reported, not quietly treated as legacy.
    ///
    /// Newest-first ordering only matters for a pre-upgrade document holding
    /// several rows, where the newest is what the old reader would have sent;
    /// every row this build writes is the document's only one.
    pub async fn get_queued_patch(
        &self,
        document_id: &Uuid,
    ) -> SyncResult<Option<(json_patch::Patch, Option<String>)>> {
        let row = sqlx::query(
            "SELECT patch, old_content_hash, base_content FROM sync_queue WHERE document_id = ? AND operation_type = 'update' ORDER BY created_at DESC, id DESC LIMIT 1"
        )
        .bind(document_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let base_json: Option<String> = row.try_get("base_content")?;
        let old_hash: Option<String> = row.try_get("old_content_hash")?;

        if let Some(base_json) = base_json {
            let base: serde_json::Value = serde_json::from_str(&base_json)?;
            let current: Option<String> = sqlx::query("SELECT content FROM documents WHERE id = ?")
                .bind(document_id.to_string())
                .fetch_optional(&self.pool)
                .await?
                .map(|row| row.try_get("content"))
                .transpose()?;

            if let Some(current) = current {
                let current: serde_json::Value = serde_json::from_str(&current)?;
                let hash = old_hash.unwrap_or_else(|| calculate_checksum(&base));
                return Ok(Some((create_patch(&base, &current)?, Some(hash))));
            }
            tracing::warn!(
                "DATABASE: Queued patch for {} has no document row, serving the stored patch",
                document_id
            );
        }

        let patch_json: Option<String> = row.try_get("patch")?;
        match patch_json {
            Some(json) => Ok(Some((serde_json::from_str(&json)?, old_hash))),
            None => Ok(None),
        }
    }

    pub async fn remove_from_sync_queue(&self, document_id: &Uuid) -> SyncResult<()> {
        sqlx::query("DELETE FROM sync_queue WHERE document_id = ?")
            .bind(document_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ===== FTS (Full-Text Search) Methods =====

    /// Configure which JSON paths to index for full-text search.
    /// Replaces existing configuration and rebuilds the index.
    pub async fn configure_search(&self, json_paths: &[String]) -> SyncResult<()> {
        // Use transaction to ensure config and index stay in sync
        let mut tx = self.pool.begin().await?;

        // Clear existing config
        sqlx::query(Queries::CLEAR_SEARCH_CONFIG)
            .execute(&mut *tx)
            .await?;

        // Insert new paths
        for path in json_paths {
            sqlx::query(Queries::INSERT_SEARCH_PATH)
                .bind(path)
                .execute(&mut *tx)
                .await?;
        }

        // Rebuild the index with new configuration (inline to use same transaction)
        sqlx::query(Queries::CLEAR_FTS_INDEX)
            .execute(&mut *tx)
            .await?;

        sqlx::query(Queries::REBUILD_FTS_INDEX)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        tracing::info!(
            "FTS: Configured search with {} paths and rebuilt index",
            json_paths.len()
        );
        Ok(())
    }

    /// Update the FTS index entry for a single document.
    /// Call this after creating or updating a document.
    pub async fn update_fts_for_document(&self, document_id: &Uuid) -> SyncResult<()> {
        // Skip FTS update if no search paths are configured
        let has_config: (i32,) = sqlx::query_as(Queries::HAS_SEARCH_CONFIG)
            .fetch_one(&self.pool)
            .await?;
        if has_config.0 == 0 {
            return Ok(());
        }

        let doc_id_str = document_id.to_string();

        // Use transaction to ensure atomicity (no orphaned entries on crash)
        let mut tx = self.pool.begin().await?;

        // Delete existing entry
        sqlx::query(Queries::DELETE_FTS_ENTRY)
            .bind(&doc_id_str)
            .execute(&mut *tx)
            .await?;

        // Insert new entry (query handles deleted_at check internally)
        sqlx::query(Queries::UPDATE_FTS_ENTRY)
            .bind(&doc_id_str)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Rebuild the entire FTS index from all documents.
    pub async fn rebuild_fts_index(&self) -> SyncResult<()> {
        // Clear existing index
        sqlx::query(Queries::CLEAR_FTS_INDEX)
            .execute(&self.pool)
            .await?;

        // Rebuild from all non-deleted documents
        sqlx::query(Queries::REBUILD_FTS_INDEX)
            .execute(&self.pool)
            .await?;

        tracing::info!("FTS: Index rebuilt");
        Ok(())
    }

    /// Search documents using FTS5 full-text search.
    /// Returns all documents matching the query.
    pub async fn search_documents(&self, query: &str, limit: i64) -> SyncResult<Vec<Document>> {
        let rows = sqlx::query(Queries::SEARCH_DOCUMENTS)
            .bind(query)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;

        rows.into_iter()
            .map(|row| DbHelpers::parse_document(&row))
            .collect()
    }
}

#[cfg(test)]
mod sync_queue_tests {
    use super::*;
    use replicant_core::patches::apply_patch;
    use serde_json::json;

    async fn fresh_db() -> ClientDatabase {
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

    async fn update_rows(db: &ClientDatabase, id: Uuid) -> i64 {
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

    async fn stored_base(db: &ClientDatabase, id: Uuid) -> Option<serde_json::Value> {
        let raw: Option<String> = sqlx::query(
            "SELECT base_content FROM sync_queue WHERE document_id = ? AND operation_type = 'update'",
        )
        .bind(id.to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .try_get("base_content")
        .unwrap();
        raw.map(|json| serde_json::from_str(&json).unwrap())
    }

    #[tokio::test]
    async fn three_offline_edits_collapse_into_one_cumulative_patch() {
        let db = fresh_db().await;
        let id = Uuid::new_v4();
        let base = json!({"title": "base", "n": 0});
        db.save_document_with_status(&doc_with(id, base.clone()), Some(SyncStatus::Synced))
            .await
            .unwrap();

        edit(&db, id, json!({"title": "base", "n": 1})).await;
        edit(&db, id, json!({"title": "edited", "n": 1})).await;
        let final_content = json!({"title": "edited", "n": 2});
        edit(&db, id, final_content.clone()).await;

        assert_eq!(
            update_rows(&db, id).await,
            1,
            "offline edits must share one queue row"
        );
        assert_eq!(
            stored_base(&db, id).await,
            Some(base.clone()),
            "the base must stay the last state the server acknowledged"
        );

        let (patch, hash) = db.get_queued_patch(&id).await.unwrap().unwrap();
        assert_eq!(hash.as_deref(), Some(calculate_checksum(&base).as_str()));

        let mut replayed = base;
        apply_patch(&mut replayed, &patch).unwrap();
        assert_eq!(
            replayed, final_content,
            "the queued patch must carry all three edits"
        );
    }

    /// A document created offline keeps its `create` row while later offline
    /// edits pile up beside it, so the flush knows to send a create. The edits
    /// still collapse into one update row, and an ack clears both — leaving the
    /// update row with nothing to fire afterwards.
    #[tokio::test]
    async fn offline_edits_leave_the_create_row_for_the_upload_to_find() {
        let db = fresh_db().await;
        let id = Uuid::new_v4();
        let created = json!({"title": "draft"});
        db.save_new_document_and_queue_create(&doc_with(id, created))
            .await
            .unwrap();

        assert!(
            db.has_queued_create(&id).await.unwrap(),
            "a locally created document starts out owing the server a create"
        );

        edit(&db, id, json!({"title": "draft", "a": 1})).await;
        let final_content = json!({"title": "draft", "a": 1, "b": 2});
        edit(&db, id, final_content.clone()).await;

        assert!(
            db.has_queued_create(&id).await.unwrap(),
            "editing must not consume the create the server still has not seen"
        );
        assert_eq!(
            update_rows(&db, id).await,
            1,
            "offline edits must still share one queue row"
        );
        assert_eq!(
            db.get_document(&id).await.unwrap().content,
            final_content,
            "the create uploads current content, so it must carry both edits"
        );

        // What the create ack does.
        db.remove_from_sync_queue(&id).await.unwrap();
        assert!(
            !db.has_queued_create(&id).await.unwrap(),
            "the ack consumes the create"
        );
        assert!(
            db.get_queued_patch(&id).await.unwrap().is_none(),
            "the ack also clears the update row, which the create already carried"
        );
    }

    /// The lost-ack trace: the server committed the create but the ack never
    /// arrived, so the create row survives. A later edit, then a resync that
    /// rebases onto the server's copy, must consume that create row — otherwise
    /// the next upload sends a create the server rejects as a duplicate, and a
    /// rejected create clears nothing.
    #[tokio::test]
    async fn rebasing_onto_server_content_consumes_the_create_row() {
        let db = fresh_db().await;
        let id = Uuid::new_v4();
        let created = json!({"title": "draft"});
        db.save_new_document_and_queue_create(&doc_with(id, created.clone()))
            .await
            .unwrap();

        // The ack was lost, so the create row is still owed. Then a local edit.
        let edited = json!({"title": "draft", "a": 1});
        edit(&db, id, edited.clone()).await;
        assert!(db.has_queued_create(&id).await.unwrap());

        // A resync delivers the server's copy and rebases the local edit onto it.
        let server_content = json!({"title": "draft", "server": true});
        let expected = DocumentPreImage {
            sync_revision: 1,
            content: edited,
            sync_status: SyncStatus::Pending,
        };
        assert!(db
            .rebase_pending_document_if_unchanged(
                &id,
                &json!({"title": "draft", "server": true, "a": 1}),
                7,
                &server_content,
                &calculate_checksum(&server_content),
                SyncStatus::Pending,
                &expected,
            )
            .await
            .unwrap());

        assert!(
            !db.has_queued_create(&id).await.unwrap(),
            "adopting server content is the create's ack"
        );
        assert!(
            db.get_queued_patch(&id).await.unwrap().is_some(),
            "the rebased edit must still be queued as an update"
        );
    }

    /// Adopting server state through the broadcast/full-sync route — a plain
    /// write at `Synced` — must also consume the create row, or a later edit
    /// uploads as a duplicate create.
    #[tokio::test]
    async fn saving_a_document_as_synced_consumes_the_create_row() {
        let db = fresh_db().await;
        let id = Uuid::new_v4();
        let created = json!({"title": "draft"});
        db.save_new_document_and_queue_create(&doc_with(id, created.clone()))
            .await
            .unwrap();

        db.save_document_with_status(&doc_with(id, created), Some(SyncStatus::Synced))
            .await
            .unwrap();

        assert!(
            !db.has_queued_create(&id).await.unwrap(),
            "a document written as Synced is one the server holds"
        );
    }

    /// The `DocumentCreated` echo settles a document with a bare `mark_synced`,
    /// so that too must consume the create row — otherwise a lost ack costs a
    /// doomed duplicate create and a spurious error event.
    #[tokio::test]
    async fn marking_a_document_synced_consumes_the_create_row() {
        let db = fresh_db().await;
        let id = Uuid::new_v4();
        db.save_new_document_and_queue_create(&doc_with(id, json!({"title": "draft"})))
            .await
            .unwrap();

        db.mark_synced(&id).await.unwrap();

        assert!(
            !db.has_queued_create(&id).await.unwrap(),
            "settling a document against the server is the create's ack"
        );
    }

    /// A document the server has acknowledged owes no create, so its later
    /// edits upload as updates.
    #[tokio::test]
    async fn an_acknowledged_document_owes_no_create() {
        let db = fresh_db().await;
        let id = Uuid::new_v4();
        let created = json!({"title": "draft"});
        db.save_new_document_and_queue_create(&doc_with(id, created))
            .await
            .unwrap();
        db.remove_from_sync_queue(&id).await.unwrap();

        edit(&db, id, json!({"title": "edited"})).await;

        assert!(!db.has_queued_create(&id).await.unwrap());
        assert!(
            db.get_queued_patch(&id).await.unwrap().is_some(),
            "the edit still queues an update"
        );
    }

    #[tokio::test]
    async fn a_legacy_row_without_a_base_falls_back_to_its_stored_patch() {
        let db = fresh_db().await;
        let id = Uuid::new_v4();
        db.save_document_with_status(
            &doc_with(id, json!({"title": "current"})),
            Some(SyncStatus::Pending),
        )
        .await
        .unwrap();

        // A row as a pre-upgrade client wrote it: a patch and a hash, no base.
        let legacy = create_patch(&json!({"title": "old"}), &json!({"title": "current"})).unwrap();
        sqlx::query(
            "INSERT INTO sync_queue (document_id, operation_type, patch, old_content_hash) VALUES (?, 'update', ?, ?)",
        )
        .bind(id.to_string())
        .bind(serde_json::to_string(&legacy).unwrap())
        .bind("legacy-hash")
        .execute(&db.pool)
        .await
        .unwrap();

        let (patch, hash) = db.get_queued_patch(&id).await.unwrap().unwrap();
        assert_eq!(patch, legacy, "the stored patch must be served verbatim");
        assert_eq!(hash.as_deref(), Some("legacy-hash"));
    }

    #[tokio::test]
    async fn a_local_edit_after_a_rebase_keeps_the_server_base() {
        let db = fresh_db().await;
        let id = Uuid::new_v4();
        let base = json!({"title": "base", "n": 0});
        db.save_document_with_status(&doc_with(id, base.clone()), Some(SyncStatus::Synced))
            .await
            .unwrap();
        edit(&db, id, json!({"title": "base", "n": 1})).await;

        // A rebase moves the base forward to fresh server content.
        let server = json!({"title": "server", "n": 0});
        let rebased = json!({"title": "server", "n": 1});
        let local = db.get_document(&id).await.unwrap();
        let expected = DocumentPreImage {
            sync_revision: local.sync_revision,
            content: local.content.clone(),
            sync_status: SyncStatus::Pending,
        };
        assert!(db
            .rebase_pending_document_if_unchanged(
                &id,
                &rebased,
                9,
                &server,
                &calculate_checksum(&server),
                SyncStatus::Pending,
                &expected,
            )
            .await
            .unwrap());

        // A later local edit extends that base; it does not reset it to the
        // content the user happened to be looking at.
        let final_content = json!({"title": "server", "n": 2});
        edit(&db, id, final_content.clone()).await;

        assert_eq!(update_rows(&db, id).await, 1);
        assert_eq!(stored_base(&db, id).await, Some(server.clone()));

        let (patch, hash) = db.get_queued_patch(&id).await.unwrap().unwrap();
        assert_eq!(hash.as_deref(), Some(calculate_checksum(&server).as_str()));
        let mut replayed = server;
        apply_patch(&mut replayed, &patch).unwrap();
        assert_eq!(replayed, final_content);
    }

    #[tokio::test]
    async fn clearing_the_queue_rebases_the_next_edit_on_the_synced_content() {
        let db = fresh_db().await;
        let id = Uuid::new_v4();
        let base = json!({"title": "base", "n": 0});
        db.save_document_with_status(&doc_with(id, base.clone()), Some(SyncStatus::Synced))
            .await
            .unwrap();

        let acked = json!({"title": "base", "n": 1});
        edit(&db, id, acked.clone()).await;
        // The ack clears the queue: the edit is now the server's state.
        db.remove_from_sync_queue(&id).await.unwrap();
        assert!(db.get_queued_patch(&id).await.unwrap().is_none());

        let next = json!({"title": "base", "n": 2});
        edit(&db, id, next.clone()).await;

        assert_eq!(stored_base(&db, id).await, Some(acked.clone()));
        let (patch, hash) = db.get_queued_patch(&id).await.unwrap().unwrap();
        assert_eq!(hash.as_deref(), Some(calculate_checksum(&acked).as_str()));
        let mut replayed = acked;
        apply_patch(&mut replayed, &patch).unwrap();
        assert_eq!(replayed, next);
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    async fn fresh_db() -> ClientDatabase {
        let db = ClientDatabase::new(":memory:").await.unwrap();
        db.run_migrations().await.unwrap();
        db
    }

    #[tokio::test]
    async fn user_config_has_identity_adopted_defaulting_to_zero() {
        let db = fresh_db().await;
        db.ensure_user_config("ws://localhost/ws").await.unwrap();

        let row = sqlx::query("SELECT identity_adopted FROM user_config LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let adopted: i64 = row.try_get("identity_adopted").unwrap();
        assert_eq!(adopted, 0);
    }

    #[tokio::test]
    async fn ensure_user_config_with_identifier_generates_random_v4_id() {
        let db = fresh_db().await;
        db.ensure_user_config_with_identifier("ws://localhost/ws", "test@example.com")
            .await
            .unwrap();

        let user_id = db.get_user_id().await.unwrap();
        // No longer derived from the email.
        assert_ne!(user_id.to_string(), "71b2b712-7878-56ee-8323-43809b8198a5");
        assert_eq!(user_id.get_version(), Some(uuid::Version::Random));

        let row = sqlx::query("SELECT identity_adopted FROM user_config LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let adopted: i64 = row.try_get("identity_adopted").unwrap();
        assert_eq!(adopted, 0);
    }

    async fn seed_document(db: &ClientDatabase, owner: Option<Uuid>) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO documents (id, user_id, content) VALUES (?1, ?2, ?3)")
            .bind(id.to_string())
            .bind(owner.map(|u| u.to_string()))
            .bind("{}")
            .execute(&db.pool)
            .await
            .unwrap();
        id
    }

    async fn count_docs_for(db: &ClientDatabase, owner: Uuid) -> i64 {
        sqlx::query("SELECT COUNT(*) as c FROM documents WHERE user_id = ?1")
            .bind(owner.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .try_get("c")
            .unwrap()
    }

    #[tokio::test]
    async fn adopt_identity_restamps_docs_and_flips_flag() {
        let db = fresh_db().await;
        db.ensure_user_config("ws://localhost/ws").await.unwrap();
        let provisional = db.get_user_id().await.unwrap();
        let canonical = Uuid::new_v4();

        seed_document(&db, Some(provisional)).await;
        seed_document(&db, Some(provisional)).await;
        let public_id = seed_document(&db, None).await;

        db.adopt_identity(provisional, canonical).await.unwrap();

        // Identity flipped in user_config.
        assert_eq!(db.get_user_id().await.unwrap(), canonical);
        let adopted: i64 = sqlx::query("SELECT identity_adopted FROM user_config LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .try_get("identity_adopted")
            .unwrap();
        assert_eq!(adopted, 1);

        // Owned documents re-stamped; none remain under the provisional id.
        assert_eq!(count_docs_for(&db, canonical).await, 2);
        assert_eq!(count_docs_for(&db, provisional).await, 0);

        // Public (null-owner) document untouched.
        let pub_null: i64 =
            sqlx::query("SELECT COUNT(*) as c FROM documents WHERE id = ?1 AND user_id IS NULL")
                .bind(public_id.to_string())
                .fetch_one(&db.pool)
                .await
                .unwrap()
                .try_get("c")
                .unwrap();
        assert_eq!(pub_null, 1);
    }

    #[tokio::test]
    async fn adopt_identity_with_no_documents_still_flips_flag() {
        let db = fresh_db().await;
        db.ensure_user_config("ws://localhost/ws").await.unwrap();
        let provisional = db.get_user_id().await.unwrap();
        let canonical = Uuid::new_v4();

        db.adopt_identity(provisional, canonical).await.unwrap();

        assert_eq!(db.get_user_id().await.unwrap(), canonical);
        let adopted: i64 = sqlx::query("SELECT identity_adopted FROM user_config LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .try_get("identity_adopted")
            .unwrap();
        assert_eq!(adopted, 1);
    }

    #[tokio::test]
    async fn adopt_identity_rejects_nil_canonical_id() {
        let db = fresh_db().await;
        db.ensure_user_config("ws://localhost/ws").await.unwrap();
        let provisional = db.get_user_id().await.unwrap();
        assert!(db.adopt_identity(provisional, Uuid::nil()).await.is_err());
    }

    #[tokio::test]
    async fn adopt_identity_rejects_second_adoption() {
        let db = fresh_db().await;
        db.ensure_user_config("ws://localhost/ws").await.unwrap();
        let provisional = db.get_user_id().await.unwrap();
        let canonical = Uuid::new_v4();
        db.adopt_identity(provisional, canonical).await.unwrap();
        assert!(db.adopt_identity(canonical, Uuid::new_v4()).await.is_err());
    }
}
