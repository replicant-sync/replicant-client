//! Store test fixtures: a temp database file with a seeded identity.

use std::path::PathBuf;

use tempfile::TempDir;
use uuid::Uuid;

use super::Store;

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
