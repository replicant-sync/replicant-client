//! Protocol v2 local store: every DB effect of the sans-IO core, one SQLite transaction each.

use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, Sqlite, SqliteConnection, SqlitePool, Transaction};
use uuid::Uuid;

use docs::{LogOrigin, Writer};

use crate::engine::doc::DocEvent;
use crate::engine::list_merge::ListMergeConfig;
use crate::engine::types::{SCOPE_CURATED, SCOPE_OWN};
use crate::queries::Queries;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const POOL_SIZE: u32 = 4;
const TOMBSTONE_RETENTION_SECS: i64 = 90 * 24 * 3600;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Uuid(#[from] uuid::Error),
    #[error("document {0} not found")]
    NotFound(Uuid),
    #[error("document {0} is not writable")]
    NotWritable(Uuid),
    #[error("document {0} already exists or was deleted")]
    AlreadyExists(Uuid),
    #[error("no user_config row")]
    NoUserConfig,
    #[error("corrupt row: {0}")]
    Corrupt(String),
    #[error("server user id must not be nil")]
    NilServerUserId,
    #[error("no field-conflict copy with id {0}")]
    NoFieldConflict(i64),
    #[error("no kept copy with id {0}")]
    NoKeptCopy(i64),
    #[error("document {0} is gone; restore its kept copy as a new document")]
    DocumentGone(Uuid),
    /// Migrating v1 sync data failed; the v1 tables are untouched and a backup sits next to
    /// the database.
    #[error("v1 data migration failed: {0}")]
    MigrationFailed(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

impl StoreError {
    /// A newer build migrated this database: it has a migration this build does not know.
    pub fn is_newer_schema(&self) -> bool {
        matches!(
            self,
            StoreError::Migrate(sqlx::migrate::MigrateError::VersionMissing(_))
        )
    }

    pub fn is_migration_failed(&self) -> bool {
        matches!(self, StoreError::MigrationFailed(_))
    }

    /// SQLITE_BUSY: another connection held the lock past the busy timeout.
    pub fn is_busy(&self) -> bool {
        // sqlx reports the extended code, whose low byte is the primary code.
        matches!(self, StoreError::Db(sqlx::Error::Database(db))
            if db.code().and_then(|code| code.parse::<i32>().ok()).is_some_and(|code| code & 0xFF == 5))
    }
}

/// The kept copy a rule wrote just before its notice (`Store::list_recovered`).
#[derive(Debug, Clone, PartialEq)]
pub struct KeptCopy {
    pub recovered_id: i64,
    /// `conflict`, `field_conflict`, `delete_wins`, `became_publication`, `create_rejected`,
    /// `delete_superseded`, `delete_refused` or `delete_publication`. `unmigratable` copies are
    /// written by the v1 migration without a notice.
    pub reason: String,
}

/// An event a rule asked for (`DocOp::Emit`), for the driver to hand to the host.
#[derive(Debug, Clone, PartialEq)]
pub struct DocNotice {
    pub doc_id: Uuid,
    pub event: DocEvent,
    pub kept: Option<KeptCopy>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IdentityCheck {
    Matches,
    /// First join: local documents and `user_config` were re-stamped from `previous`.
    Adopted {
        previous: Uuid,
    },
    /// This data dir already adopted `local`; a different server id is fatal.
    Drift {
        local: Uuid,
    },
}

pub struct Store {
    pool: SqlitePool,
    options: SqliteConnectOptions,
    instance_id: Uuid,
    /// How the per-document rules merge lists; the engine sets it from its config.
    pub(crate) list_merge: ListMergeConfig,
}

impl Store {
    pub async fn open(path: &Path) -> StoreResult<Store> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(BUSY_TIMEOUT);
        let pool = SqlitePoolOptions::new()
            .max_connections(POOL_SIZE)
            .connect_with(options.clone())
            .await?;
        if let Err(error) = prepare(&pool, path).await {
            pool.close().await;
            return Err(error);
        }
        Ok(Store {
            pool,
            options,
            instance_id: Uuid::new_v4(),
            list_merge: ListMergeConfig::default(),
        })
    }

    /// Awaits every pooled connection's shutdown.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// Identifies this engine's writes in the change log; new on every open.
    pub fn instance_id(&self) -> Uuid {
        self.instance_id
    }

    pub(crate) fn writer(&self, origin: LogOrigin) -> Writer {
        Writer {
            instance_id: self.instance_id,
            origin,
        }
    }

    pub async fn user_id(&self) -> StoreResult<Uuid> {
        read_user_id(&mut *self.pool.acquire().await?).await
    }

    /// Gives a new data dir a provisional identity (the server's id is adopted at the first
    /// join) and returns the data dir's client id.
    pub async fn ensure_user_config(&self, server_url: &str) -> StoreResult<Uuid> {
        let mut tx = self.begin().await?;
        let existing: Option<String> =
            sqlx::query_scalar("SELECT client_id FROM user_config LIMIT 1")
                .fetch_optional(&mut *tx)
                .await?;
        let client_id = match existing {
            Some(client_id) => Uuid::parse_str(&client_id)?,
            None => {
                let client_id = Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO user_config (user_id, client_id, server_url, identity_adopted) \
                     VALUES (?, ?, ?, 0)",
                )
                .bind(Uuid::new_v4().to_string())
                .bind(client_id.to_string())
                .bind(server_url)
                .execute(&mut *tx)
                .await?;
                client_id
            }
        };
        tx.commit().await?;
        Ok(client_id)
    }

    /// The join's user id against this data dir: adopt it if none was ever adopted.
    pub async fn check_identity(&self, server_user_id: Uuid) -> StoreResult<IdentityCheck> {
        if server_user_id.is_nil() {
            return Err(StoreError::NilServerUserId);
        }
        let mut tx = self.begin().await?;
        let row = sqlx::query("SELECT user_id, identity_adopted FROM user_config LIMIT 1")
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NoUserConfig)?;
        let local = Uuid::parse_str(&row.try_get::<String, _>("user_id")?)?;
        let adopted = row.try_get::<i64, _>("identity_adopted")? != 0;
        let check = if local == server_user_id {
            if !adopted {
                sqlx::query(Queries::ADOPT_USER_CONFIG_IDENTITY)
                    .bind(server_user_id.to_string())
                    .bind(local.to_string())
                    .execute(&mut *tx)
                    .await?;
            }
            IdentityCheck::Matches
        } else if !adopted {
            sqlx::query(Queries::RESTAMP_DOCUMENTS_USER_ID)
                .bind(server_user_id.to_string())
                .bind(local.to_string())
                .execute(&mut *tx)
                .await?;
            sqlx::query(Queries::ADOPT_USER_CONFIG_IDENTITY)
                .bind(server_user_id.to_string())
                .bind(local.to_string())
                .execute(&mut *tx)
                .await?;
            IdentityCheck::Adopted { previous: local }
        } else {
            IdentityCheck::Drift { local }
        };
        tx.commit().await?;
        Ok(check)
    }

    /// A write transaction. `IMMEDIATE` takes the write lock up front, so a read-then-write
    /// never fails with `SQLITE_BUSY` when another process committed in between.
    pub(crate) async fn begin(&self) -> StoreResult<Transaction<'static, Sqlite>> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
    }
}

async fn prepare(pool: &SqlitePool, path: &Path) -> StoreResult<()> {
    v1_data::back_up_v1_database(pool, path).await?;
    sqlx::migrate!("./migrations").run(pool).await?;
    v1_data::migrate_v1_data(pool).await?;
    sqlx::query("INSERT OR IGNORE INTO subscriptions (scope) VALUES (?), (?)")
        .bind(SCOPE_OWN)
        .bind(SCOPE_CURATED)
        .execute(pool)
        .await?;
    let now = now_unix();
    sqlx::query("DELETE FROM tombstones WHERE deleted_at < ?")
        .bind(now - TOMBSTONE_RETENTION_SECS)
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `documents` timestamps stay RFC 3339: v1 readers parse them.
pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// The data dir's user. Writes read it inside their own transaction: another process's first
/// join may have re-stamped it since this process last looked.
pub(crate) async fn read_user_id(conn: &mut SqliteConnection) -> StoreResult<Uuid> {
    let user_id: Option<String> = sqlx::query_scalar("SELECT user_id FROM user_config LIMIT 1")
        .fetch_optional(&mut *conn)
        .await?;
    Ok(Uuid::parse_str(&user_id.ok_or(StoreError::NoUserConfig)?)?)
}

pub mod change_log;
mod docs;
mod feed;
mod recovered;
mod uploads;
mod v1_data;
mod writes;

pub use recovered::RecoveredCopy;

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[tokio::test]
    async fn ensure_user_config_creates_one_provisional_row_and_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("fresh.sqlite3"))
            .await
            .unwrap();
        let first = store.ensure_user_config("ws://one").await.unwrap();
        let again = store.ensure_user_config("ws://two").await.unwrap();
        assert_eq!(first, again, "one client id per data dir");
        assert_eq!(count(&store, "SELECT COUNT(*) FROM user_config").await, 1);
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM user_config WHERE identity_adopted = 0"
            )
            .await,
            1,
            "the server's id is adopted at the first join"
        );
        store.user_id().await.unwrap();
        store.close().await;
    }

    #[tokio::test]
    async fn a_database_from_a_newer_build_is_reported_as_newer_schema() {
        let t = temp_store().await;
        exec(
            &t.store,
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (99, 'from a newer build', 1, x'00', 0)",
        )
        .await;
        t.store.close().await;
        let error = Store::open(&t.path())
            .await
            .err()
            .expect("a newer schema must not open");
        assert!(error.is_newer_schema(), "{error}");
    }

    #[tokio::test]
    async fn open_applies_the_v2_schema_in_wal_mode() {
        let t = temp_store().await;
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&t.store.pool)
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        for table in [
            "outbox",
            "change_log",
            "change_log_readers",
            "subscriptions",
            "doc_scopes",
            "tombstones",
            "recovered",
        ] {
            let sql = format!(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '{table}'"
            );
            assert_eq!(count(&t.store, &sql).await, 1, "{table}");
        }
        let cursors: Vec<(String, i64)> =
            sqlx::query_as("SELECT scope, cursor FROM subscriptions ORDER BY scope")
                .fetch_all(&t.store.pool)
                .await
                .unwrap();
        assert_eq!(
            cursors,
            vec![
                ("collection:curated".to_string(), 0),
                ("own".to_string(), 0)
            ]
        );
    }

    #[tokio::test]
    async fn first_join_adopts_the_server_identity_then_later_ids_drift() {
        let t = temp_store().await;
        let provisional = Uuid::from_u128(0x1);
        let server = Uuid::from_u128(0x2);
        seed_user(&t.store, provisional, false).await;
        sqlx::query(
            "INSERT INTO documents (id, user_id, content, created_at, updated_at) \
             VALUES (?, ?, '{}', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00')",
        )
        .bind(Uuid::from_u128(0xD0C1).to_string())
        .bind(provisional.to_string())
        .execute(&t.store.pool)
        .await
        .unwrap();

        assert_eq!(
            t.store.check_identity(server).await.unwrap(),
            IdentityCheck::Adopted {
                previous: provisional
            }
        );
        assert_eq!(t.store.user_id().await.unwrap(), server);
        let owned_by_server = format!("SELECT COUNT(*) FROM documents WHERE user_id = '{server}'");
        assert_eq!(count(&t.store, &owned_by_server).await, 1);
        assert_eq!(
            t.store.check_identity(server).await.unwrap(),
            IdentityCheck::Matches
        );
        assert_eq!(
            t.store.check_identity(Uuid::from_u128(0x3)).await.unwrap(),
            IdentityCheck::Drift { local: server }
        );
    }

    #[tokio::test]
    async fn matching_first_join_marks_the_identity_adopted() {
        let t = temp_store().await;
        seed_user(&t.store, ME, false).await;
        assert_eq!(
            t.store.check_identity(ME).await.unwrap(),
            IdentityCheck::Matches
        );
        assert_eq!(
            t.store.check_identity(Uuid::from_u128(0x3)).await.unwrap(),
            IdentityCheck::Drift { local: ME }
        );
    }

    #[tokio::test]
    async fn check_identity_rejects_a_nil_server_user_id() {
        let t = temp_store().await;
        assert!(matches!(
            t.store.check_identity(Uuid::nil()).await,
            Err(StoreError::NilServerUserId)
        ));
    }

    #[tokio::test]
    async fn open_trims_expired_tombstones_but_keeps_recovered_copies() {
        let t = temp_store().await;
        let now = now_unix();
        sqlx::query(
            "INSERT INTO tombstones (doc_id, server_seq, deleted_at) VALUES ('old', 1, ?), ('new', 2, ?)",
        )
        .bind(now - 91 * 24 * 3600)
        .bind(now)
        .execute(&t.store.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO recovered (doc_id, content, reason, recovered_at) VALUES ('old', '{}', 'conflict', ?)",
        )
        .bind(now - 31 * 24 * 3600)
        .execute(&t.store.pool)
        .await
        .unwrap();
        t.store.close().await;

        let reopened = Store::open(&t.path()).await.unwrap();
        assert_eq!(count(&reopened, "SELECT COUNT(*) FROM tombstones").await, 1);
        assert_eq!(
            count(&reopened, "SELECT COUNT(*) FROM recovered").await,
            1,
            "a kept copy stays until the user dismisses it"
        );
    }

    #[tokio::test]
    async fn default_subscriptions_come_back_on_open() {
        let t = temp_store().await;
        sqlx::query("DELETE FROM subscriptions WHERE scope = 'collection:curated'")
            .execute(&t.store.pool)
            .await
            .unwrap();
        t.store.close().await;
        let reopened = Store::open(&t.path()).await.unwrap();
        assert_eq!(
            count(&reopened, "SELECT COUNT(*) FROM subscriptions").await,
            2
        );
    }
}
