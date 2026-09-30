//! Migration 015: v1 sync data into the v2 shadow and outbox model. SQLite has no SHA-256 and
//! no UUIDv7, so this runs in Rust after the SQL migrations, in one transaction. The v1
//! `sync_queue` table is its gate: the step drops it, so it runs once per data dir.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sqlx::{Row, SqliteConnection, SqlitePool};
use tracing::{info, warn};
use uuid::Uuid;

use super::{now_unix, StoreError, StoreResult};
use crate::engine::hash::{canonicalise_numbers, content_hash};
use crate::engine::types::{SCOPE_CURATED, SCOPE_OWN};

#[derive(Default)]
pub(crate) struct Counts {
    synced: usize,
    hard_deleted: usize,
    pending: usize,
    pending_without_base: usize,
    rejected_creates: usize,
    unmigratable: usize,
    kept_aside: usize,
    dropped_queue_rows: usize,
    markers: u64,
}

struct QueueRow {
    kind: &'static str,
    base: Option<Value>,
    created_at: Option<i64>,
}

pub(crate) async fn migrate_v1_data(pool: &SqlitePool) -> StoreResult<Counts> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !holds_v1_data(&mut *tx).await? {
        tx.commit().await?;
        return Ok(Counts::default());
    }
    let counts = migrate(&mut tx).await.map_err(migration_failed)?;
    tx.commit()
        .await
        .map_err(|error| migration_failed(error.into()))?;
    info!(
        synced = counts.synced,
        hard_deleted = counts.hard_deleted,
        pending = counts.pending,
        rejected_creates = counts.rejected_creates,
        unmigratable = counts.unmigratable,
        kept_aside = counts.kept_aside,
        dropped_queue_rows = counts.dropped_queue_rows,
        markers = counts.markers,
        "migrated v1 sync data"
    );
    if counts.dropped_queue_rows > 0 {
        warn!(
            rows = counts.dropped_queue_rows,
            "v1 queue rows dropped: their documents are missing, set aside, or kept aside"
        );
    }
    if counts.kept_aside > 0 {
        warn!(
            documents = counts.kept_aside,
            "v1 local work that cannot be uploaded: each document's content is kept aside"
        );
    }
    if counts.pending_without_base > 0 {
        warn!(
            documents = counts.pending_without_base,
            "v1 edits with no known base: each is kept aside if the server's copy differs"
        );
    }
    Ok(counts)
}

async fn migrate(conn: &mut SqliteConnection) -> StoreResult<Counts> {
    let me = sqlx::query_scalar::<_, String>("SELECT user_id FROM user_config LIMIT 1")
        .fetch_optional(&mut *conn)
        .await?
        .map(|user_id| Uuid::parse_str(&user_id))
        .transpose()?;
    // Without the account there is no telling whose documents are pending edits, and
    // treating them all as someone else's would let the curated sweep remove them.
    if me.is_none()
        && sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM documents")
            .fetch_one(&mut *conn)
            .await?
            > 0
    {
        return Err(StoreError::MigrationFailed(
            "the v1 database holds documents but no user_config row".into(),
        ));
    }

    let mut queue: HashMap<Uuid, Vec<QueueRow>> = HashMap::new();
    let queued = sqlx::query(
        "SELECT document_id, operation_type, base_content, \
         CAST(strftime('%s', created_at) AS INTEGER) AS created_unix FROM sync_queue ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut counts = Counts::default();
    for row in queued {
        let Ok(doc_id) = Uuid::parse_str(&row.try_get::<String, _>("document_id")?) else {
            counts.dropped_queue_rows += 1;
            continue;
        };
        let base_content: Option<String> = row.try_get("base_content")?;
        // An unknown operation or an unreadable base still leaves an edit to send, with no base.
        let (kind, base_content) = match row.try_get::<String, _>("operation_type")?.as_str() {
            "create" => ("create", base_content),
            "update" => ("update", base_content),
            "delete" => ("delete", base_content),
            _ => ("update", None),
        };
        queue.entry(doc_id).or_default().push(QueueRow {
            kind,
            base: base_content.and_then(|base| serde_json::from_str(&base).ok()),
            created_at: row.try_get("created_unix")?,
        });
    }
    // The queue references documents, so it goes before any document is hard-deleted.
    sqlx::query("DROP TABLE sync_queue")
        .execute(&mut *conn)
        .await?;

    let documents = sqlx::query(
        "SELECT id, user_id, content, sync_status, deleted_at, visibility, \
         CAST(strftime('%s', deleted_at) AS INTEGER) AS deleted_unix FROM documents ORDER BY rowid",
    )
    .fetch_all(&mut *conn)
    .await?;
    let migrated_at_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
    let now = now_unix();
    for document in documents {
        let stored_id: String = document.try_get("id")?;
        let raw_content: String = document.try_get("content")?;
        let owner = match document.try_get::<Option<String>, _>("user_id")? {
            None => Some(None),
            Some(owner) => Uuid::parse_str(&owner).ok().map(Some),
        };
        let (Some(doc_id), Some(owner), Some(mut content)) = (
            Uuid::parse_str(&stored_id).ok(),
            owner,
            serde_json::from_str::<Value>(&raw_content).ok(),
        ) else {
            set_aside_unmigratable(&mut *conn, &stored_id, &raw_content, &mut counts).await?;
            continue;
        };
        let id = doc_id.to_string();
        // Two v1 ids that differ only in case: the one already in canonical form (else the
        // first renamed) keeps the id.
        if stored_id != id && document_exists(&mut *conn, &id).await? {
            set_aside_unmigratable(&mut *conn, &stored_id, &raw_content, &mut counts).await?;
            continue;
        }
        if stored_id != id {
            sqlx::query("UPDATE documents SET id = ? WHERE id = ?")
                .bind(&id)
                .bind(&stored_id)
                .execute(&mut *conn)
                .await?;
            sqlx::query("UPDATE documents_fts SET document_id = ? WHERE document_id = ?")
                .bind(&id)
                .bind(&stored_id)
                .execute(&mut *conn)
                .await?;
        }
        canonicalise_numbers(&mut content);
        let status = document
            .try_get::<Option<String>, _>("sync_status")?
            .unwrap_or_default();
        let deleted = document
            .try_get::<Option<String>, _>("deleted_at")?
            .is_some();
        let public = document
            .try_get::<Option<String>, _>("visibility")?
            .as_deref()
            == Some("public");
        let rows = queue.remove(&doc_id).unwrap_or_default();
        let mine = owner.is_some() && owner == me;
        let has_create = rows.iter().any(|row| row.kind == "create");
        let pending = mine && status == "pending";
        let rejected_create = mine && status == "conflict" && has_create;
        // Only the owner's pending work becomes uploads. Anything else queued, and content only
        // this device holds on a document that is not ours, is kept aside. Only a local delete
        // (always `pending` in v1) is the user's own choice; a delete from elsewhere is `synced`.
        if !pending && !rejected_create {
            counts.dropped_queue_rows += rows.len();
            let local_only = !mine && (status == "pending" || (status == "conflict" && has_create));
            if !(deleted && status == "pending") && (local_only || !rows.is_empty()) {
                keep_aside(&mut *conn, &id, &content, "unmigratable").await?;
                counts.kept_aside += 1;
            }
        }

        if deleted && !pending {
            let deleted_at = document
                .try_get::<Option<i64>, _>("deleted_unix")?
                .unwrap_or(now);
            hard_delete(&mut *conn, &id, deleted_at).await?;
            counts.hard_deleted += 1;
            continue;
        }

        let (shadow, markers) = if pending {
            counts.pending += 1;
            // With a shadow the first upload would be an update of a document the server
            // may never have had.
            let shadow = if has_create {
                None
            } else {
                rows.iter()
                    .rev()
                    .find_map(|row| row.base.clone())
                    .map(|mut base| {
                        canonicalise_numbers(&mut base);
                        base
                    })
            };
            // Edits and deletes with no known base; a plain v1 delete has no queue row at all.
            if shadow.is_none() && !has_create && (deleted || !rows.is_empty()) {
                counts.pending_without_base += 1;
            }
            let mut markers: Vec<_> = rows
                .iter()
                .map(|row| (row.kind, row.created_at.unwrap_or(now)))
                .collect();
            if deleted {
                markers.push(("delete", now));
            } else if markers.is_empty() {
                markers.push(("create", now));
            }
            (shadow, markers)
        } else if rejected_create {
            // The v2 engine's end state for a refused create: content kept, document gone.
            counts.rejected_creates += 1;
            keep_aside(&mut *conn, &id, &content, "create_rejected").await?;
            remove_document(&mut *conn, &id).await?;
            continue;
        } else {
            counts.synced += 1;
            (Some(content.clone()), Vec::new())
        };

        // Documents of no or another account start read-only, as their snapshot would make them.
        sqlx::query(
            "UPDATE documents SET content = ?, hash = ?, server_content = ?, server_hash = ?, \
             server_seq = ?, read_only = ? WHERE id = ?",
        )
        .bind(content.to_string())
        .bind(content_hash(&content))
        .bind(shadow.as_ref().map(Value::to_string))
        .bind(shadow.as_ref().map(content_hash))
        .bind(shadow.as_ref().map(|_| 0_i64))
        .bind(!mine)
        .bind(&id)
        .execute(&mut *conn)
        .await?;
        for (kind, created_at) in markers {
            counts.markers += 1;
            sqlx::query(
                "INSERT INTO outbox (mutation_id, doc_id, kind, created_at) VALUES (?, ?, ?, ?)",
            )
            .bind(migrated_mutation_id(migrated_at_ms, counts.markers).to_string())
            .bind(&id)
            .bind(kind)
            .bind(created_at)
            .execute(&mut *conn)
            .await?;
        }
        // The own snapshot does not list publications: the author's public document is also
        // curated, or own's sweep would remove it until the curated page brings it back.
        let scopes: &[&str] = match (mine, public) {
            (true, true) => &[SCOPE_OWN, SCOPE_CURATED],
            (true, false) => &[SCOPE_OWN],
            (false, _) => &[SCOPE_CURATED],
        };
        for scope in scopes {
            sqlx::query(
                "INSERT OR IGNORE INTO doc_scopes (doc_id, scope, member, seq) VALUES (?, ?, 1, 0)",
            )
            .bind(&id)
            .bind(scope)
            .execute(&mut *conn)
            .await?;
        }
    }

    // What is left belongs to documents that are missing or were set aside.
    counts.dropped_queue_rows += queue.values().map(Vec::len).sum::<usize>();

    sqlx::query("DROP TABLE IF EXISTS change_events")
        .execute(&mut *conn)
        .await?;
    sqlx::query("DROP TABLE IF EXISTS sync_state")
        .execute(&mut *conn)
        .await?;
    sqlx::query("ALTER TABLE documents DROP COLUMN local_changes")
        .execute(&mut *conn)
        .await?;
    Ok(counts)
}

async fn hard_delete(conn: &mut SqliteConnection, id: &str, deleted_at: i64) -> StoreResult<()> {
    remove_document(&mut *conn, id).await?;
    sqlx::query(
        "INSERT OR IGNORE INTO tombstones (doc_id, server_seq, deleted_at) VALUES (?, 0, ?)",
    )
    .bind(id)
    .bind(deleted_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Deletes the row and its search entry; no tombstone, so a server copy can come back.
async fn remove_document(conn: &mut SqliteConnection, id: &str) -> StoreResult<()> {
    sqlx::query("DELETE FROM documents WHERE id = ?")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(super::reads::DELETE_FTS_ENTRY)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn keep_aside(
    conn: &mut SqliteConnection,
    doc_id: &str,
    content: &Value,
    reason: &str,
) -> StoreResult<()> {
    sqlx::query(
        "INSERT INTO recovered (doc_id, content, reason, recovered_at) VALUES (?, ?, ?, ?)",
    )
    .bind(doc_id)
    .bind(content.to_string())
    .bind(reason)
    .bind(now_unix())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// A v1 row 015 cannot read: its content is kept (as a JSON string when it is not JSON) under
/// a readable id, and the row goes.
async fn set_aside_unmigratable(
    conn: &mut SqliteConnection,
    stored_id: &str,
    raw_content: &str,
    counts: &mut Counts,
) -> StoreResult<()> {
    let content = serde_json::from_str::<Value>(raw_content)
        .unwrap_or_else(|_| Value::String(raw_content.to_string()));
    let doc_id = Uuid::parse_str(stored_id).unwrap_or_else(|_| Uuid::new_v4());
    warn!(v1_id = stored_id, kept_as = %doc_id, "v1 document could not be migrated; kept aside");
    keep_aside(&mut *conn, &doc_id.to_string(), &content, "unmigratable").await?;
    remove_document(&mut *conn, stored_id).await?;
    counts.unmigratable += 1;
    Ok(())
}

async fn document_exists(conn: &mut SqliteConnection, id: &str) -> StoreResult<bool> {
    let found: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM documents WHERE id = ?")
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
    Ok(found > 0)
}

/// SQLITE_BUSY stays as it is, so `retry_open` retries it; anything else is this migration's
/// failure, which retrying cannot fix.
fn migration_failed(error: StoreError) -> StoreError {
    if error.is_busy() || error.is_migration_failed() {
        error
    } else {
        StoreError::MigrationFailed(error.to_string())
    }
}

async fn holds_v1_data<'c>(conn: impl sqlx::SqliteExecutor<'c>) -> StoreResult<bool> {
    let v1_tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'sync_queue'",
    )
    .fetch_one(conn)
    .await?;
    Ok(v1_tables > 0)
}

/// Copies a data dir that still holds v1 sync data to `<db path>.v1-backup` before 015 changes
/// it for good. The copy is written under a temporary name and renamed, so a backup that exists
/// is complete. An existing backup is never replaced: a later attempt writes a new one beside it
/// (`backup_path`).
pub(crate) async fn back_up_v1_database(pool: &SqlitePool, db_path: &Path) -> StoreResult<()> {
    if !holds_v1_data(pool).await? {
        return Ok(());
    }
    // Holding the write lock serialises backups across processes, so a temporary file found
    // here was left by a crash.
    let mut lock = pool.begin_with("BEGIN IMMEDIATE").await?;
    // Another process may have migrated the file while this one waited for the lock.
    if !holds_v1_data(&mut *lock).await? {
        lock.rollback().await?;
        return Ok(());
    }
    let backup = backup_path(db_path, now_unix());
    let partial = sibling(db_path, ".v1-backup.tmp");
    let written = write_backup(pool, &partial, &backup).await;
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    lock.rollback().await?;
    written
}

async fn write_backup(pool: &SqlitePool, partial: &Path, backup: &Path) -> StoreResult<()> {
    match std::fs::remove_file(partial) {
        Ok(()) => warn!(path = %partial.display(), "removed a partial v1 backup"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(backup_failed(error)),
    }
    // A second pooled connection: VACUUM cannot run inside the lock's transaction.
    sqlx::query("VACUUM INTO ?")
        .bind(partial.to_string_lossy().into_owned())
        .execute(pool)
        .await
        .map_err(|error| match StoreError::from(error) {
            busy if busy.is_busy() => busy,
            error => backup_failed(error),
        })?;
    std::fs::File::open(partial)
        .and_then(|file| file.sync_all())
        .map_err(backup_failed)?;
    std::fs::rename(partial, backup).map_err(backup_failed)?;
    if let Some(dir) = backup.parent() {
        // Best effort: makes the rename durable where directories can be synced.
        let _ = std::fs::File::open(dir).and_then(|dir| dir.sync_all());
    }
    Ok(())
}

fn backup_failed(error: impl std::fmt::Display) -> StoreError {
    StoreError::MigrationFailed(format!("backup before migrating: {error}"))
}

/// The first free name of `<db path>.v1-backup`, `.v1-backup-<unix seconds>`,
/// `.v1-backup-<unix seconds>-2`, … Chosen under the write lock, so no other backup races it.
fn backup_path(db_path: &Path, now: i64) -> PathBuf {
    let first = sibling(db_path, ".v1-backup");
    if !first.exists() {
        return first;
    }
    let stamped = format!(".v1-backup-{now}");
    let mut candidate = sibling(db_path, &stamped);
    let mut n = 2;
    while candidate.exists() {
        candidate = sibling(db_path, &format!("{stamped}-{n}"));
        n += 1;
    }
    candidate
}

fn sibling(db_path: &Path, suffix: &str) -> PathBuf {
    let mut path = db_path.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

/// UUIDv7 at `unix_ms` with `counter` in the low bits: strictly increasing in migration
/// order; `insert_marker` keeps every later id of a document above them.
fn migrated_mutation_id(unix_ms: u64, counter: u64) -> Uuid {
    let timestamp = u128::from(unix_ms & 0xFFFF_FFFF_FFFF) << 80;
    let version = 0x7_u128 << 76;
    let variant = 0b10_u128 << 62;
    Uuid::from_u128(timestamp | version | variant | u128::from(counter & 0x3FFF_FFFF_FFFF_FFFF))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::{json, Value};
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::SqlitePool;
    use uuid::Uuid;

    use crate::engine::doc::DocEvent;
    use crate::engine::doc_upload::{build_upload, BuildResult};
    use crate::engine::hash::content_hash;
    use crate::engine::types::{UploadKind, SCOPE_CURATED, SCOPE_OWN};
    use crate::store::test_support::*;
    use crate::store::{now_unix, Store};

    use super::{back_up_v1_database, backup_path};

    const OTHER: Uuid = Uuid::from_u128(0xB);

    fn doc(n: u128) -> Uuid {
        Uuid::from_u128(0xD000 + n)
    }

    fn id(n: u128) -> String {
        doc(n).to_string()
    }

    /// Opens the v1 file with the v2 store, which applies 013 and 014 and runs 015.
    async fn migrated(pool: SqlitePool, path: &Path) -> Store {
        pool.close().await;
        Store::open(path).await.unwrap()
    }

    async fn outbox(store: &Store, doc_id: Uuid) -> Vec<(String, Option<String>)> {
        sqlx::query_as(
            "SELECT kind, parked_error FROM outbox WHERE doc_id = ? ORDER BY mutation_id",
        )
        .bind(doc_id.to_string())
        .fetch_all(&store.pool)
        .await
        .unwrap()
    }

    fn kinds(rows: &[&str]) -> Vec<(String, Option<String>)> {
        rows.iter().map(|kind| (kind.to_string(), None)).collect()
    }

    /// A migrated pending v1 update of doc(1); keep the dir alive as long as the store.
    async fn pending_update(content: Value, base: Option<Value>) -> (tempfile::TempDir, Store) {
        let (dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), content, "pending", None).await;
        v1_queue_row(&pool, &id(1), "update", base).await;
        (dir, migrated(pool, &path).await)
    }

    #[tokio::test]
    async fn synced_documents_get_a_seq_0_shadow_equal_to_their_content_and_no_markers() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 1}), "synced", None).await;
        let store = migrated(pool, &path).await;
        let snap = snapshot(&store, doc(1)).await;
        let shadow = snap.shadow.expect("a synced document has a shadow");
        assert_eq!((shadow.content.clone(), shadow.seq), (json!({"n": 1}), 0));
        assert_eq!(shadow.hash, content_hash(&json!({"n": 1})));
        assert!(snap.rows.is_empty());
        assert!(kept_copies(&store).await.is_empty());
        store.close().await;
    }

    #[tokio::test]
    async fn synced_deleted_documents_are_hard_deleted_and_tombstoned_at_seq_0() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        let yesterday = now_unix() - 86_400;
        let deleted_at = chrono::DateTime::from_timestamp(yesterday, 0)
            .unwrap()
            .to_rfc3339();
        v1_doc(
            &pool,
            &id(1),
            Some(ME),
            json!({"n": 1}),
            "synced",
            Some(&deleted_at),
        )
        .await;
        let store = migrated(pool, &path).await;
        let snap = snapshot(&store, doc(1)).await;
        assert!(!snap.exists, "a v2 soft delete would mean an unsent delete");
        assert_eq!(snap.tombstone_seq, Some(0));
        assert!(snap.rows.is_empty());
        let tombstoned_at: i64 =
            sqlx::query_scalar("SELECT deleted_at FROM tombstones WHERE doc_id = ?")
                .bind(id(1))
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(tombstoned_at, yesterday, "the v1 deletion time");
        assert!(kept_copies(&store).await.is_empty(), "the user deleted it");
        store.close().await;
    }

    #[tokio::test]
    async fn a_pending_create_gets_no_shadow_even_when_a_row_has_base_content() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 2}), "pending", None).await;
        v1_queue_row(&pool, &id(1), "create", None).await;
        v1_queue_row(&pool, &id(1), "update", Some(json!({"n": 1}))).await;
        let store = migrated(pool, &path).await;
        assert!(
            snapshot(&store, doc(1)).await.shadow.is_none(),
            "a shadow would upload an update of a document the server may never have had"
        );
        assert_eq!(outbox(&store, doc(1)).await, kinds(&["create", "update"]));
        store.close().await;
    }

    #[tokio::test]
    async fn a_pending_update_with_a_base_gets_that_base_as_a_seq_0_shadow() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 3}), "pending", None).await;
        v1_queue_row(&pool, &id(1), "update", Some(json!({"n": 0}))).await;
        v1_queue_row(&pool, &id(1), "update", Some(json!({"n": 1}))).await;
        let store = migrated(pool, &path).await;
        let shadow = snapshot(&store, doc(1))
            .await
            .shadow
            .expect("based on v1 base_content");
        assert_eq!(shadow.content, json!({"n": 1}), "the newest row's base");
        assert_eq!(shadow.hash, content_hash(&json!({"n": 1})));
        assert_eq!(
            shadow.seq, 0,
            "seq 0 is the migrated-shadow exemption (rule e)"
        );
        assert_eq!(outbox(&store, doc(1)).await, kinds(&["update", "update"]));
        store.close().await;
    }

    #[tokio::test]
    async fn a_pending_update_without_a_base_gets_no_shadow() {
        let (_dir, store) = pending_update(json!({"n": 2}), None).await;
        assert!(snapshot(&store, doc(1)).await.shadow.is_none());
        assert_eq!(outbox(&store, doc(1)).await, kinds(&["update"]));
        store.close().await;
    }

    #[tokio::test]
    async fn a_pending_document_with_no_rows_gets_one_create() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 1}), "pending", None).await;
        let store = migrated(pool, &path).await;
        assert!(snapshot(&store, doc(1)).await.shadow.is_none());
        assert_eq!(outbox(&store, doc(1)).await, kinds(&["create"]));
        store.close().await;
    }

    #[tokio::test]
    async fn a_pending_deleted_document_gets_its_rows_then_a_delete() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        let deleted_at = chrono::Utc::now().to_rfc3339();
        v1_doc(
            &pool,
            &id(1),
            Some(ME),
            json!({"n": 1}),
            "pending",
            Some(&deleted_at),
        )
        .await;
        v1_queue_row(&pool, &id(1), "update", Some(json!({"n": 0}))).await;
        let store = migrated(pool, &path).await;
        let snap = snapshot(&store, doc(1)).await;
        assert!(
            snap.exists && snap.soft_deleted,
            "the delete is still to be sent"
        );
        assert_eq!(outbox(&store, doc(1)).await, kinds(&["update", "delete"]));
        store.close().await;
    }

    #[tokio::test]
    async fn a_settled_conflict_is_synced_and_a_rejected_create_is_kept_aside() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 1}), "conflict", None).await;
        v1_doc(&pool, &id(2), Some(ME), json!({"n": 2}), "conflict", None).await;
        v1_queue_row(&pool, &id(2), "create", None).await;
        let store = migrated(pool, &path).await;
        let settled = snapshot(&store, doc(1)).await;
        assert_eq!(settled.shadow.map(|s| s.content), Some(json!({"n": 1})));
        assert!(settled.rows.is_empty());
        let rejected = snapshot(&store, doc(2)).await;
        assert!(!rejected.exists && rejected.tombstone_seq.is_none());
        assert!(outbox(&store, doc(2)).await.is_empty());
        let kept = store.list_recovered().await.unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(
            (kept[0].doc_id, kept[0].reason.as_str()),
            (doc(2), "create_rejected")
        );
        assert_eq!(kept[0].content, json!({"n": 2}));
        store.close().await;
    }

    #[tokio::test]
    async fn mutation_ids_are_v7_lowercase_hyphenated_and_increase_in_queue_order() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 2}), "pending", None).await;
        v1_queue_row(&pool, &id(1), "create", None).await;
        v1_queue_row(&pool, &id(1), "update", None).await;
        v1_queue_row(&pool, &id(1), "update", None).await;
        let store = migrated(pool, &path).await;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT mutation_id, kind FROM outbox WHERE doc_id = ? ORDER BY mutation_id",
        )
        .bind(id(1))
        .fetch_all(&store.pool)
        .await
        .unwrap();
        let parsed: Vec<Uuid> = rows
            .iter()
            .map(|(mutation_id, _)| Uuid::parse_str(mutation_id).unwrap())
            .collect();
        assert!(parsed
            .iter()
            .all(|mutation_id| mutation_id.get_version_num() == 7));
        for ((text, _), mutation_id) in rows.iter().zip(&parsed) {
            assert_eq!(
                *text,
                mutation_id.to_string(),
                "stored lowercase hyphenated"
            );
        }
        let mut numeric = parsed.clone();
        numeric.sort();
        assert_eq!(numeric, parsed, "text order is id order");
        let queue_order: Vec<&str> = rows.iter().map(|(_, kind)| kind.as_str()).collect();
        assert_eq!(queue_order, vec!["create", "update", "update"]);

        store
            .update_document(doc(1), json!({"n": 3}))
            .await
            .unwrap();
        let newest: String = sqlx::query_scalar(
            "SELECT mutation_id FROM outbox WHERE doc_id = ? ORDER BY mutation_id DESC LIMIT 1",
        )
        .bind(id(1))
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert!(
            newest > rows.last().unwrap().0,
            "a later edit sorts after every migrated row"
        );
        store.close().await;
    }

    #[tokio::test]
    async fn every_document_is_placed_in_own_or_curated_at_seq_0() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), json!({}), "synced", None).await;
        v1_doc(&pool, &id(2), Some(OTHER), json!({}), "synced", None).await;
        v1_doc(&pool, &id(3), None, json!({}), "synced", None).await;
        let store = migrated(pool, &path).await;
        let scopes: Vec<(String, String, i64, i64)> =
            sqlx::query_as("SELECT doc_id, scope, member, seq FROM doc_scopes ORDER BY doc_id")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(
            scopes,
            vec![
                (id(1), "own".to_string(), 1, 0),
                (id(2), "collection:curated".to_string(), 1, 0),
                (id(3), "collection:curated".to_string(), 1, 0),
            ]
        );
        let read_only: Vec<String> =
            sqlx::query_scalar("SELECT id FROM documents WHERE read_only = 1 ORDER BY id")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(
            read_only,
            vec![id(2), id(3)],
            "only our documents are writable"
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM subscriptions WHERE cursor = 0"
            )
            .await,
            2
        );
        store.close().await;
    }

    #[tokio::test]
    async fn the_v1_tables_are_gone_and_a_second_open_changes_nothing() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 1}), "pending", None).await;
        v1_queue_row(&pool, &id(1), "update", Some(json!({"n": 0}))).await;
        let store = migrated(pool, &path).await;
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE name IN ('sync_queue', 'change_events', 'sync_state')"
            )
            .await,
            0
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM pragma_table_info('documents') WHERE name = 'local_changes'"
            )
            .await,
            0
        );
        store.close().await;
        let reopened = Store::open(&path).await.unwrap();
        assert_eq!(count(&reopened, "SELECT COUNT(*) FROM outbox").await, 1);
        reopened.close().await;
    }

    #[tokio::test]
    async fn uppercase_v1_document_ids_are_normalised() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        let upper = id(1).to_uppercase();
        v1_doc(&pool, &upper, Some(ME), json!({"n": 1}), "pending", None).await;
        v1_queue_row(&pool, &upper, "update", Some(json!({"n": 0}))).await;
        let store = migrated(pool, &path).await;
        let snap = snapshot(&store, doc(1)).await;
        assert!(snap.exists);
        assert_eq!(snap.rows.len(), 1);
        let still_upper = format!("SELECT COUNT(*) FROM documents WHERE id = '{upper}'");
        assert_eq!(count(&store, &still_upper).await, 0);
        store.close().await;
    }

    #[tokio::test]
    async fn a_migrated_update_with_no_base_is_kept_aside_and_reported_on_its_first_differing_envelope(
    ) {
        let (_dir, store) = pending_update(json!({"n": 2}), None).await;
        let server = envelope(doc(1), Some(ME), json!({"n": 5}), 5);
        let notices = store
            .apply_changes(ME, SCOPE_OWN, &[upsert_change(SCOPE_OWN, server)], 5)
            .await
            .unwrap();
        assert!(
            notices
                .iter()
                .any(|notice| notice.doc_id == doc(1) && notice.event == DocEvent::ConflictDetected),
            "never silent: the host is told"
        );
        let kept: Vec<(String, String)> =
            sqlx::query_as("SELECT content, reason FROM recovered WHERE doc_id = ?")
                .bind(id(1))
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(
            kept,
            vec![(json!({"n": 2}).to_string(), "conflict".to_string())]
        );
        assert_eq!(snapshot(&store, doc(1)).await.content, json!({"n": 5}));
        store.close().await;
    }

    #[tokio::test]
    async fn a_migrated_update_with_no_base_adopts_an_envelope_with_equal_content() {
        let (_dir, store) = pending_update(json!({"n": 2}), None).await;
        let server = envelope(doc(1), Some(ME), json!({"n": 2}), 5);
        let notices = store
            .apply_changes(ME, SCOPE_OWN, &[upsert_change(SCOPE_OWN, server)], 5)
            .await
            .unwrap();
        assert!(notices.is_empty());
        let snap = snapshot(&store, doc(1)).await;
        assert_eq!(snap.shadow.map(|s| s.seq), Some(5));
        assert!(snap.rows.is_empty(), "nothing was really pending");
        assert_eq!(count(&store, "SELECT COUNT(*) FROM recovered").await, 0);
        store.close().await;
    }

    #[tokio::test]
    async fn a_migrated_update_with_a_base_rebases_onto_the_first_envelope() {
        let (_dir, store) =
            pending_update(json!({"a": 2, "b": 1}), Some(json!({"a": 1, "b": 1}))).await;
        let server = envelope(doc(1), Some(ME), json!({"a": 1, "b": 3}), 5);
        let notices = store
            .apply_changes(ME, SCOPE_OWN, &[upsert_change(SCOPE_OWN, server)], 5)
            .await
            .unwrap();
        assert!(notices.is_empty());
        let snap = snapshot(&store, doc(1)).await;
        assert_eq!(
            snap.content,
            json!({"a": 2, "b": 3}),
            "local edit kept, server edit merged"
        );
        assert_eq!(snap.rows.len(), 1, "the local edit is still to be uploaded");
        assert_eq!(count(&store, "SELECT COUNT(*) FROM recovered").await, 0);
        store.close().await;
    }

    #[tokio::test]
    async fn migrated_content_and_shadows_hold_canonical_numbers() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(
            &pool,
            &id(1),
            Some(ME),
            json!({"n": 1200.0, "x": 1.5}),
            "synced",
            None,
        )
        .await;
        v1_doc(
            &pool,
            &id(2),
            Some(ME),
            json!({"n": 1300.0}),
            "pending",
            None,
        )
        .await;
        v1_queue_row(&pool, &id(2), "update", Some(json!({"n": 1100.0}))).await;
        let store = migrated(pool, &path).await;
        let synced = snapshot(&store, doc(1)).await;
        assert_eq!(
            synced.content,
            json!({"n": 1200, "x": 1.5}),
            "1200.0 is stored as 1200"
        );
        assert_eq!(synced.shadow.unwrap().content, json!({"n": 1200, "x": 1.5}));
        let stored: String = sqlx::query_scalar("SELECT content FROM documents WHERE id = ?")
            .bind(id(1))
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            stored, r#"{"n":1200,"x":1.5}"#,
            "written back, not only read canonically"
        );
        let pending = snapshot(&store, doc(2)).await;
        assert_eq!(pending.content, json!({"n": 1300}));
        let base = pending.shadow.expect("based on v1 base_content");
        assert_eq!(base.content, json!({"n": 1100}));
        assert_eq!(base.hash, content_hash(&json!({"n": 1100})));
        store.close().await;
    }

    /// A migrated pending delete of doc(1): content {"n": 1}, one v1 update row with `base`.
    async fn pending_delete(base: Option<Value>) -> (tempfile::TempDir, Store) {
        let (dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        let deleted_at = chrono::Utc::now().to_rfc3339();
        v1_doc(
            &pool,
            &id(1),
            Some(ME),
            json!({"n": 1}),
            "pending",
            Some(&deleted_at),
        )
        .await;
        v1_queue_row(&pool, &id(1), "update", base).await;
        (dir, migrated(pool, &path).await)
    }

    #[tokio::test]
    async fn a_migrated_pending_delete_with_a_base_is_sent_on_that_base() {
        let (_dir, store) = pending_delete(Some(json!({"n": 0}))).await;
        match build_upload(&snapshot(&store, doc(1)).await, ME) {
            BuildResult::Send { upload, .. } => {
                assert_eq!(upload.kind, UploadKind::Delete);
                assert_eq!(upload.base_hash, Some(content_hash(&json!({"n": 0}))));
            }
            other => panic!("expected a conditional delete, got {other:?}"),
        }
        store.close().await;
    }

    #[tokio::test]
    async fn a_migrated_pending_delete_without_a_base_fetches_the_server_copy_first() {
        let (_dir, store) = pending_delete(None).await;
        assert_eq!(
            build_upload(&snapshot(&store, doc(1)).await, ME),
            BuildResult::NeedsServerCopy
        );
        store.close().await;
    }

    #[tokio::test]
    async fn a_plain_v1_offline_delete_with_no_queue_row_fetches_the_server_copy_first() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        let deleted_at = chrono::Utc::now().to_rfc3339();
        v1_doc(
            &pool,
            &id(1),
            Some(ME),
            json!({"n": 1}),
            "pending",
            Some(&deleted_at),
        )
        .await;
        let store = migrated(pool, &path).await;
        assert_eq!(outbox(&store, doc(1)).await, kinds(&["delete"]));
        assert_eq!(
            build_upload(&snapshot(&store, doc(1)).await, ME),
            BuildResult::NeedsServerCopy
        );
        store.close().await;
    }

    #[tokio::test]
    async fn a_migrated_create_then_delete_is_decided_by_the_first_snapshot_that_holds_it() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        let deleted_at = chrono::Utc::now().to_rfc3339();
        v1_doc(
            &pool,
            &id(1),
            Some(ME),
            json!({"n": 1}),
            "pending",
            Some(&deleted_at),
        )
        .await;
        v1_queue_row(&pool, &id(1), "create", None).await;
        let store = migrated(pool, &path).await;
        let server = envelope(doc(1), Some(ME), json!({"n": 7}), 5);
        let notices = store
            .apply_snapshot_page(ME, SCOPE_OWN, std::slice::from_ref(&server))
            .await
            .unwrap();
        assert!(notices
            .iter()
            .any(|notice| notice.event == DocEvent::DeleteSuperseded));
        let snap = snapshot(&store, doc(1)).await;
        assert!(snap.exists && !snap.soft_deleted && snap.rows.is_empty());
        assert_eq!(
            snap.content,
            json!({"n": 7}),
            "another device's edit survives"
        );
        store.close().await;
    }

    #[tokio::test]
    async fn a_migrated_pending_delete_without_a_base_is_superseded_by_different_server_content() {
        let (_dir, store) = pending_delete(None).await;
        let server = envelope(doc(1), Some(ME), json!({"n": 5}), 5);
        let notices = store
            .apply_snapshot_page(ME, SCOPE_OWN, std::slice::from_ref(&server))
            .await
            .unwrap();
        assert!(notices
            .iter()
            .any(|notice| notice.doc_id == doc(1) && notice.event == DocEvent::DeleteSuperseded));
        let snap = snapshot(&store, doc(1)).await;
        assert!(
            snap.exists && !snap.soft_deleted && snap.rows.is_empty(),
            "the document is back"
        );
        assert_eq!(snap.content, json!({"n": 5}));
        let kept: Vec<(String, String)> =
            sqlx::query_as("SELECT content, reason FROM recovered WHERE doc_id = ?")
                .bind(id(1))
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(
            kept,
            vec![(json!({"n": 1}).to_string(), "delete_superseded".to_string())],
            "with no base, a superseded delete always keeps the local content"
        );
        store.close().await;
    }

    fn backup_of(path: &Path) -> PathBuf {
        PathBuf::from(format!("{}.v1-backup", path.display()))
    }

    async fn raw_pool(path: &Path) -> SqlitePool {
        SqlitePool::connect_with(SqliteConnectOptions::new().filename(path))
            .await
            .unwrap()
    }

    async fn has_table(pool: &SqlitePool, name: &str) -> bool {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sqlite_master WHERE name = ?")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
            > 0
    }

    #[tokio::test]
    async fn a_v1_database_is_backed_up_before_it_is_migrated() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 1}), "synced", None).await;
        let store = migrated(pool, &path).await;
        store.close().await;
        let backup = raw_pool(&backup_of(&path)).await;
        assert!(
            has_table(&backup, "sync_queue").await,
            "the copy is the v1 file"
        );
        let documents: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM documents")
            .fetch_one(&backup)
            .await
            .unwrap();
        assert_eq!(documents, 1);
        backup.close().await;
        let written = std::fs::metadata(backup_of(&path))
            .unwrap()
            .modified()
            .unwrap();
        Store::open(&path).await.unwrap().close().await;
        assert_eq!(
            std::fs::metadata(backup_of(&path))
                .unwrap()
                .modified()
                .unwrap(),
            written,
            "a later open leaves the backup alone"
        );
    }

    #[tokio::test]
    async fn a_failed_v1_migration_is_reported_and_leaves_the_v1_data_in_place() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(
            &pool,
            &id(1),
            Some(ME),
            json!({}),
            "synced",
            Some("2026-01-03T00:00:00+00:00"),
        )
        .await;
        sqlx::query(
            "CREATE TRIGGER refuse BEFORE DELETE ON documents BEGIN SELECT RAISE(ABORT, 'refused'); END",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
        let error = Store::open(&path).await.err().expect("015 must fail");
        assert!(error.is_migration_failed(), "{error}");
        let raw = raw_pool(&path).await;
        assert!(has_table(&raw, "sync_queue").await, "015 rolled back");
        raw.close().await;
        assert!(backup_of(&path).exists());
    }

    #[tokio::test]
    async fn v1_documents_with_no_user_config_fail_the_migration_and_leave_the_v1_data_in_place() {
        let (_dir, path, pool) = v012_db().await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 1}), "pending", None).await;
        pool.close().await;
        let error = Store::open(&path).await.err().expect("015 must fail");
        assert!(error.is_migration_failed(), "{error}");
        let raw = raw_pool(&path).await;
        assert!(has_table(&raw, "sync_queue").await, "015 rolled back");
        raw.close().await;
        assert!(backup_of(&path).exists());
    }

    #[tokio::test]
    async fn an_empty_v1_database_with_no_user_config_still_migrates() {
        let (_dir, path, pool) = v012_db().await;
        let store = migrated(pool, &path).await;
        store.close().await;
        let raw = raw_pool(&path).await;
        assert!(!has_table(&raw, "sync_queue").await);
        raw.close().await;
    }

    /// Runs the v1 migration on `pool`'s file and returns what it counted.
    async fn migrated_with_counts(pool: SqlitePool, path: &Path) -> (Store, super::Counts) {
        pool.close().await;
        let raw = raw_pool(path).await;
        sqlx::migrate!("./migrations").run(&raw).await.unwrap();
        let counts = super::migrate_v1_data(&raw).await.unwrap();
        raw.close().await;
        (Store::open(path).await.unwrap(), counts)
    }

    #[tokio::test]
    async fn queue_rows_of_missing_or_unmigratable_documents_are_counted_as_dropped() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 1}), "pending", None).await;
        v1_queue_row(&pool, &id(1), "update", None).await;
        // A document whose content is not JSON is set aside, and so is its queue row.
        v1_doc(&pool, &id(2), Some(ME), json!("x"), "pending", None).await;
        sqlx::query("UPDATE documents SET content = 'not json' WHERE id = ?")
            .bind(id(2))
            .execute(&pool)
            .await
            .unwrap();
        v1_queue_row(&pool, &id(2), "update", None).await;
        // No document 3 exists.
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&pool)
            .await
            .unwrap();
        v1_queue_row(&pool, &id(3), "update", None).await;
        let (store, counts) = migrated_with_counts(pool, &path).await;
        store.close().await;
        assert_eq!(counts.dropped_queue_rows, 2);
    }

    fn partial_backup_of(path: &Path) -> PathBuf {
        PathBuf::from(format!("{}.v1-backup.tmp", path.display()))
    }

    async fn backed_up_content(path: &Path, doc_id: Uuid) -> String {
        let backup = raw_pool(&backup_of(path)).await;
        assert!(
            has_table(&backup, "sync_queue").await,
            "the copy is the v1 file"
        );
        let content = sqlx::query_scalar("SELECT content FROM documents WHERE id = ?")
            .bind(doc_id.to_string())
            .fetch_one(&backup)
            .await
            .unwrap();
        backup.close().await;
        content
    }

    #[tokio::test]
    async fn a_partial_backup_left_by_a_crash_is_replaced_by_a_complete_one() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 1}), "synced", None).await;
        std::fs::write(partial_backup_of(&path), b"half a database").unwrap();
        migrated(pool, &path).await.close().await;
        assert!(!partial_backup_of(&path).exists());
        assert_eq!(
            backed_up_content(&path, doc(1)).await,
            json!({"n": 1}).to_string()
        );
    }

    #[tokio::test]
    async fn a_backup_from_an_earlier_failed_migration_is_kept_as_it_was() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(
            &pool,
            &id(1),
            Some(ME),
            json!({"n": 1}),
            "synced",
            Some("2026-01-03T00:00:00+00:00"),
        )
        .await;
        sqlx::query(
            "CREATE TRIGGER refuse BEFORE DELETE ON documents BEGIN SELECT RAISE(ABORT, 'refused'); END",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
        assert!(Store::open(&path)
            .await
            .err()
            .unwrap()
            .is_migration_failed());

        let raw = raw_pool(&path).await;
        sqlx::query("DROP TRIGGER refuse")
            .execute(&raw)
            .await
            .unwrap();
        sqlx::query("UPDATE documents SET content = '{\"n\":2}'")
            .execute(&raw)
            .await
            .unwrap();
        raw.close().await;
        let first = std::fs::read(backup_of(&path)).unwrap();
        Store::open(&path).await.unwrap().close().await;
        assert_eq!(
            std::fs::read(backup_of(&path)).unwrap(),
            first,
            "the retry leaves the first backup byte for byte"
        );
        assert_eq!(
            backed_up_content(&path, doc(1)).await,
            json!({"n": 1}).to_string()
        );
        let prefix = format!("{}.v1-backup-", path.file_name().unwrap().to_string_lossy());
        let stamped: Vec<PathBuf> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .strip_prefix(&prefix)
                    .is_some_and(|secs| secs.parse::<i64>().is_ok())
            })
            .collect();
        assert_eq!(stamped.len(), 1, "the retry writes a timestamped backup");
        let second = raw_pool(&stamped[0]).await;
        let content: String = sqlx::query_scalar("SELECT content FROM documents")
            .fetch_one(&second)
            .await
            .unwrap();
        second.close().await;
        assert_eq!(content, json!({"n": 2}).to_string());
    }

    #[test]
    fn a_backup_name_already_taken_gets_the_next_free_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lib.sqlite3");
        let named = |suffix: &str| dir.path().join(format!("lib.sqlite3{suffix}"));
        assert_eq!(backup_path(&db, 7), named(".v1-backup"));
        std::fs::write(named(".v1-backup"), b"a").unwrap();
        assert_eq!(backup_path(&db, 7), named(".v1-backup-7"));
        std::fs::write(named(".v1-backup-7"), b"b").unwrap();
        assert_eq!(backup_path(&db, 7), named(".v1-backup-7-2"));
        std::fs::write(named(".v1-backup-7-2"), b"c").unwrap();
        assert_eq!(backup_path(&db, 7), named(".v1-backup-7-3"));
    }

    #[tokio::test]
    async fn a_backup_blocked_by_another_writer_is_busy_and_succeeds_once_it_lets_go() {
        use sqlx::sqlite::SqlitePoolOptions;
        use sqlx::ConnectOptions;

        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 1}), "synced", None).await;
        pool.close().await;
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .busy_timeout(std::time::Duration::from_millis(1));
        let mut writer = options.connect().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut writer)
            .await
            .unwrap();
        let opener = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();

        let error = back_up_v1_database(&opener, &path).await.unwrap_err();
        assert!(error.is_busy(), "retry_open retries it: {error}");
        assert!(!backup_of(&path).exists());
        assert!(!partial_backup_of(&path).exists());

        sqlx::query("ROLLBACK").execute(&mut writer).await.unwrap();
        back_up_v1_database(&opener, &path).await.unwrap();
        opener.close().await;
        assert_eq!(
            backed_up_content(&path, doc(1)).await,
            json!({"n": 1}).to_string()
        );
    }

    #[tokio::test]
    async fn a_backup_that_waited_for_the_lock_does_not_copy_a_migrated_file() {
        use sqlx::sqlite::SqlitePoolOptions;
        use sqlx::ConnectOptions;

        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        pool.close().await;
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .busy_timeout(std::time::Duration::from_secs(10));
        let mut other = options.connect().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut other)
            .await
            .unwrap();
        let opener = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        let backup_path = path.clone();
        let backup = tokio::spawn(async move {
            let result = back_up_v1_database(&opener, &backup_path).await;
            opener.close().await;
            result
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        // Another process migrates the file while the backup waits for the lock.
        sqlx::query("DROP TABLE sync_queue")
            .execute(&mut other)
            .await
            .unwrap();
        sqlx::query("COMMIT").execute(&mut other).await.unwrap();
        backup.await.unwrap().unwrap();
        assert!(!backup_of(&path).exists());
        assert!(!partial_backup_of(&path).exists());
    }

    #[tokio::test]
    async fn unreadable_v1_documents_are_kept_aside_and_the_rest_migrate() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(
            &pool,
            "not-a-uuid",
            Some(ME),
            json!({"a": 1}),
            "synced",
            None,
        )
        .await;
        v1_doc(&pool, &id(2), Some(ME), json!({}), "synced", None).await;
        sqlx::query("UPDATE documents SET content = 'not json' WHERE id = ?")
            .bind(id(2))
            .execute(&pool)
            .await
            .unwrap();
        v1_doc(&pool, &id(3), Some(ME), json!({"n": 3}), "synced", None).await;
        let store = migrated(pool, &path).await;
        assert!(snapshot(&store, doc(3)).await.exists);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM documents").await, 1);
        let mut kept: Vec<(Value, String)> = store
            .list_recovered()
            .await
            .unwrap()
            .into_iter()
            .map(|copy| (copy.content, copy.reason))
            .collect();
        kept.sort_by_key(|(content, _)| content.to_string());
        assert_eq!(
            kept,
            vec![
                (json!("not json"), "unmigratable".to_string()),
                (json!({"a": 1}), "unmigratable".to_string()),
            ]
        );
        store.close().await;
    }

    #[tokio::test]
    async fn a_case_colliding_v1_id_keeps_the_canonical_row_and_sets_the_other_aside() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(
            &pool,
            &id(1).to_uppercase(),
            Some(ME),
            json!({"n": 2}),
            "synced",
            None,
        )
        .await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 1}), "synced", None).await;
        let store = migrated(pool, &path).await;
        assert_eq!(snapshot(&store, doc(1)).await.content, json!({"n": 1}));
        let kept = store.list_recovered().await.unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(
            (kept[0].content.clone(), kept[0].reason.as_str()),
            (json!({"n": 2}), "unmigratable")
        );
        store.close().await;
    }

    #[tokio::test]
    async fn an_unknown_queue_operation_becomes_an_edit_with_no_base() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 1}), "pending", None).await;
        // v1's CHECK allows only create/update/delete; a damaged file may hold anything.
        sqlx::query("PRAGMA ignore_check_constraints = ON")
            .execute(&pool)
            .await
            .unwrap();
        v1_queue_row(&pool, &id(1), "patch", Some(json!({"n": 0}))).await;
        v1_doc(&pool, &id(2), Some(ME), json!({"n": 2}), "pending", None).await;
        v1_queue_row(&pool, &id(2), "update", None).await;
        sqlx::query("UPDATE sync_queue SET base_content = 'not json' WHERE document_id = ?")
            .bind(id(2))
            .execute(&pool)
            .await
            .unwrap();
        let store = migrated(pool, &path).await;
        for n in [1, 2] {
            assert_eq!(outbox(&store, doc(n)).await, kinds(&["update"]));
            assert!(snapshot(&store, doc(n)).await.shadow.is_none());
        }
        store.close().await;
    }

    #[tokio::test]
    async fn an_authors_public_document_is_also_placed_in_curated() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 1}), "synced", None).await;
        sqlx::query("UPDATE documents SET visibility = 'public' WHERE id = ?")
            .bind(id(1))
            .execute(&pool)
            .await
            .unwrap();
        let store = migrated(pool, &path).await;
        let mut scopes: Vec<String> = snapshot(&store, doc(1))
            .await
            .memberships
            .into_iter()
            .map(|membership| membership.scope)
            .collect();
        scopes.sort();
        assert_eq!(
            scopes,
            vec![SCOPE_CURATED.to_string(), SCOPE_OWN.to_string()]
        );
        store.close().await;
    }

    async fn kept_copies(store: &Store) -> Vec<(Uuid, Value, String)> {
        store
            .list_recovered()
            .await
            .unwrap()
            .into_iter()
            .map(|copy| (copy.doc_id, copy.content, copy.reason))
            .collect()
    }

    #[tokio::test]
    async fn a_pending_edit_on_another_accounts_document_is_kept_aside_and_it_migrates_as_synced() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(OTHER), json!({"n": 1}), "pending", None).await;
        v1_queue_row(&pool, &id(1), "update", None).await;
        let store = migrated(pool, &path).await;
        let snap = snapshot(&store, doc(1)).await;
        assert!(snap.rows.is_empty());
        assert!(snap.read_only, "not ours to edit");
        assert_eq!(snap.shadow.map(|s| s.content), Some(json!({"n": 1})));
        assert_eq!(
            kept_copies(&store).await,
            vec![(doc(1), json!({"n": 1}), "unmigratable".to_string())]
        );
        store.close().await;
    }

    /// A pending v1 edit of doc(1) with no owner, as v1's `resync_document` inserts a document it
    /// did not have.
    async fn unowned_pending_edit() -> (tempfile::TempDir, Store) {
        let (dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), None, json!({"n": 2}), "pending", None).await;
        v1_queue_row(&pool, &id(1), "update", Some(json!({"n": 1}))).await;
        (dir, migrated(pool, &path).await)
    }

    #[tokio::test]
    async fn a_pending_edit_on_an_unowned_document_is_kept_before_the_curated_sweep() {
        let (_dir, store) = unowned_pending_edit().await;
        assert!(snapshot(&store, doc(1)).await.read_only);
        store.finish_snapshot(SCOPE_CURATED, &[], 1).await.unwrap();
        assert!(!snapshot(&store, doc(1)).await.exists);
        assert_eq!(
            kept_copies(&store).await,
            vec![(doc(1), json!({"n": 2}), "unmigratable".to_string())]
        );
        store.close().await;
    }

    #[tokio::test]
    async fn a_pending_edit_on_an_unowned_document_is_kept_when_a_snapshot_replaces_it() {
        let (_dir, store) = unowned_pending_edit().await;
        let server = envelope(doc(1), Some(ME), json!({"n": 1}), 5);
        store
            .apply_snapshot_page(ME, SCOPE_OWN, std::slice::from_ref(&server))
            .await
            .unwrap();
        assert_eq!(snapshot(&store, doc(1)).await.content, json!({"n": 1}));
        assert_eq!(
            kept_copies(&store).await,
            vec![(doc(1), json!({"n": 2}), "unmigratable".to_string())]
        );
        store.close().await;
    }

    #[tokio::test]
    async fn the_identity_adoption_state_is_carried_over() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, false).await;
        let store = migrated(pool, &path).await;
        assert_eq!(store.user_id().await.unwrap(), ME);
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM user_config WHERE identity_adopted = 0"
            )
            .await,
            1,
            "the first join still adopts or checks it"
        );
        store.close().await;
    }

    fn kept(n: u128, content: Value) -> Vec<(Uuid, Value, String)> {
        vec![(doc(n), content, "unmigratable".to_string())]
    }

    #[tokio::test]
    async fn a_conflict_with_a_queued_update_keeps_its_content_aside_and_drops_the_rows() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 2}), "conflict", None).await;
        v1_queue_row(&pool, &id(1), "update", Some(json!({"n": 1}))).await;
        v1_queue_row(&pool, &id(1), "update", None).await;
        let (store, counts) = migrated_with_counts(pool, &path).await;
        assert!(outbox(&store, doc(1)).await.is_empty());
        assert_eq!(kept_copies(&store).await, kept(1, json!({"n": 2})));
        assert_eq!(counts.dropped_queue_rows, 2);
        store.close().await;
    }

    #[tokio::test]
    async fn a_refused_create_on_another_accounts_document_keeps_its_content_aside() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(
            &pool,
            &id(1),
            Some(OTHER),
            json!({"n": 3}),
            "conflict",
            None,
        )
        .await;
        v1_queue_row(&pool, &id(1), "create", None).await;
        let (store, counts) = migrated_with_counts(pool, &path).await;
        let snap = snapshot(&store, doc(1)).await;
        assert!(snap.read_only && snap.rows.is_empty());
        assert_eq!(kept_copies(&store).await, kept(1, json!({"n": 3})));
        assert_eq!(counts.dropped_queue_rows, 1);
        store.close().await;
    }

    #[tokio::test]
    async fn a_synced_document_with_queue_rows_keeps_its_content_aside_and_drops_the_rows() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(&pool, &id(1), Some(ME), json!({"n": 4}), "synced", None).await;
        v1_queue_row(&pool, &id(1), "update", None).await;
        let (store, counts) = migrated_with_counts(pool, &path).await;
        let snap = snapshot(&store, doc(1)).await;
        assert!(snap.rows.is_empty());
        assert_eq!(snap.shadow.map(|s| s.content), Some(json!({"n": 4})));
        assert_eq!(kept_copies(&store).await, kept(1, json!({"n": 4})));
        assert_eq!(counts.dropped_queue_rows, 1);
        store.close().await;
    }

    #[tokio::test]
    async fn a_pending_delete_on_another_accounts_document_keeps_no_copy() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(
            &pool,
            &id(1),
            Some(OTHER),
            json!({"n": 5}),
            "pending",
            Some("2026-01-03T00:00:00+00:00"),
        )
        .await;
        v1_queue_row(&pool, &id(1), "delete", None).await;
        let (store, counts) = migrated_with_counts(pool, &path).await;
        assert!(!snapshot(&store, doc(1)).await.exists);
        assert!(kept_copies(&store).await.is_empty());
        assert_eq!(counts.dropped_queue_rows, 1);
        store.close().await;
    }

    #[tokio::test]
    async fn a_document_deleted_elsewhere_keeps_its_unsent_edit_aside() {
        let (_dir, path, pool) = v012_db().await;
        v1_user(&pool, ME, true).await;
        v1_doc(
            &pool,
            &id(1),
            Some(ME),
            json!({"n": 2}),
            "synced",
            Some("2026-01-03T00:00:00+00:00"),
        )
        .await;
        v1_queue_row(&pool, &id(1), "update", Some(json!({"n": 1}))).await;
        let (store, counts) = migrated_with_counts(pool, &path).await;
        assert!(!snapshot(&store, doc(1)).await.exists);
        assert_eq!(kept_copies(&store).await, kept(1, json!({"n": 2})));
        assert_eq!(counts.dropped_queue_rows, 1);
        store.close().await;
    }
}
