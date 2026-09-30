//! Store test fixtures: a temp database file with a seeded identity.

use std::path::PathBuf;

use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use tempfile::TempDir;
use uuid::Uuid;

use super::docs::{apply_ops, load_snapshot, LogOrigin};
use super::{DocNotice, Store};
use crate::engine::doc::{DocOp, DocSnapshot, Membership, Shadow};
use crate::engine::hash::content_hash;
use crate::engine::types::{Change, ChangeKind, DocEnvelope, Seq};

pub(crate) const ME: Uuid = Uuid::from_u128(0xA);
const DB_FILE: &str = "replicant.sqlite3";

pub(crate) struct TempStore {
    pub dir: TempDir,
    pub store: Store,
}

impl TempStore {
    pub fn path(&self) -> PathBuf {
        self.dir.path().join(DB_FILE)
    }
}

pub(crate) async fn temp_store() -> TempStore {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join(DB_FILE)).await.unwrap();
    seed_user(&store, ME, true).await;
    TempStore { dir, store }
}

pub(crate) async fn seed_user(store: &Store, user_id: Uuid, adopted: bool) {
    sqlx::query("DELETE FROM user_config")
        .execute(&store.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO user_config (user_id, client_id, server_url, identity_adopted) \
         VALUES (?, ?, 'ws://test', ?)",
    )
    .bind(user_id.to_string())
    .bind(Uuid::new_v4().to_string())
    .bind(adopted as i64)
    .execute(&store.pool)
    .await
    .unwrap();
}

pub(crate) async fn count(store: &Store, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&store.pool)
        .await
        .unwrap()
}

pub(crate) async fn snapshot(store: &Store, doc_id: Uuid) -> DocSnapshot {
    let mut tx = store.begin().await.unwrap();
    let snap = load_snapshot(&mut tx, doc_id).await.unwrap();
    tx.commit().await.unwrap();
    snap
}

/// Runs `rule` on the stored snapshot and writes its ops, as a DB effect does.
pub(crate) async fn apply(
    store: &Store,
    doc_id: Uuid,
    rule: impl FnOnce(&DocSnapshot) -> Vec<DocOp>,
) -> Vec<DocNotice> {
    apply_with_envelope(store, doc_id, None, rule).await
}

pub(crate) async fn apply_with_envelope(
    store: &Store,
    doc_id: Uuid,
    envelope: Option<&DocEnvelope>,
    rule: impl FnOnce(&DocSnapshot) -> Vec<DocOp>,
) -> Vec<DocNotice> {
    let mut tx = store.begin().await.unwrap();
    let snap = load_snapshot(&mut tx, doc_id).await.unwrap();
    let ops = rule(&snap);
    let notices = apply_ops(
        &mut tx,
        &store.writer(LogOrigin::Server),
        &snap,
        &ops,
        envelope,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    notices
}

/// A document the server acknowledged at `seq`, member of `scope`, with no pending rows.
pub(crate) async fn seed_synced(
    store: &Store,
    doc_id: Uuid,
    scope: &str,
    owner_id: Option<Uuid>,
    content: Value,
    seq: Seq,
) {
    let scope = scope.to_string();
    apply(store, doc_id, move |_| {
        vec![
            DocOp::SetMembership(Membership {
                scope,
                member: true,
                seq,
            }),
            DocOp::SetMeta {
                owner_id,
                read_only: false,
            },
            DocOp::SetShadow(Shadow {
                hash: content_hash(&content),
                content: content.clone(),
                seq,
            }),
            DocOp::SetContent(content),
        ]
    })
    .await;
}

pub(crate) fn envelope(
    doc_id: Uuid,
    owner_id: Option<Uuid>,
    content: Value,
    seq: Seq,
) -> DocEnvelope {
    DocEnvelope {
        doc_id,
        owner_id,
        author_id: None,
        read_only: false,
        source_doc_id: None,
        derived_from: None,
        title: None,
        hash: content_hash(&content),
        content,
        seq,
    }
}

/// Another engine on the same file, as a second process opens it.
pub(crate) async fn open_again(path: &std::path::Path) -> Store {
    Store::open(path).await.unwrap()
}

pub(crate) fn upsert_change(scope: &str, doc: DocEnvelope) -> Change {
    Change {
        scope: scope.to_string(),
        seq: doc.seq,
        prev_seq: doc.seq - 1,
        doc_id: doc.doc_id,
        kind: ChangeKind::Upsert,
        doc: Some(doc),
        client_id: None,
        upload_id: None,
    }
}

pub(crate) async fn exec(store: &Store, sql: &str) {
    sqlx::query(sql).execute(&store.pool).await.unwrap();
}

pub(crate) fn delete_change(scope: &str, doc_id: Uuid, seq: Seq) -> Change {
    Change {
        scope: scope.to_string(),
        seq,
        prev_seq: seq - 1,
        doc_id,
        kind: ChangeKind::Delete,
        doc: None,
        client_id: None,
        upload_id: None,
    }
}

/// A data dir at the last v1 schema (migration 012), for migration 015 tests. Write v1 rows
/// through the returned pool, close it, then `Store::open` the path.
pub(crate) async fn v012_db() -> (TempDir, PathBuf, SqlitePool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true),
        )
        .await
        .unwrap();
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.migrations = std::borrow::Cow::Owned(
        migrator
            .iter()
            .filter(|migration| migration.version <= 12)
            .cloned()
            .collect(),
    );
    migrator.run(&pool).await.unwrap();
    (dir, path, pool)
}

pub(crate) async fn v1_user(pool: &SqlitePool, user_id: Uuid, adopted: bool) {
    sqlx::query(
        "INSERT INTO user_config (user_id, client_id, server_url, identity_adopted) \
         VALUES (?, ?, 'ws://v1', ?)",
    )
    .bind(user_id.to_string())
    .bind(Uuid::new_v4().to_string())
    .bind(adopted as i64)
    .execute(pool)
    .await
    .unwrap();
}

/// A v1 `documents` row, with `id` stored exactly as given.
pub(crate) async fn v1_doc(
    pool: &SqlitePool,
    id: &str,
    owner: Option<Uuid>,
    content: Value,
    status: &str,
    deleted_at: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO documents (id, user_id, content, sync_revision, created_at, updated_at, \
         deleted_at, sync_status, title) \
         VALUES (?, ?, ?, 1, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00', ?, ?, 'v1')",
    )
    .bind(id)
    .bind(owner.map(|owner| owner.to_string()))
    .bind(content.to_string())
    .bind(deleted_at)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

pub(crate) async fn v1_queue_row(pool: &SqlitePool, doc_id: &str, kind: &str, base: Option<Value>) {
    sqlx::query(
        "INSERT INTO sync_queue (document_id, operation_type, patch, base_content, created_at) \
         VALUES (?, ?, '[]', ?, '2026-01-02 03:04:05')",
    )
    .bind(doc_id)
    .bind(kind)
    .bind(base.map(|base| base.to_string()))
    .execute(pool)
    .await
    .unwrap();
}
