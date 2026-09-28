//! Cross-process change notification: the change log read by position, woken by
//! `PRAGMA data_version` on a dedicated connection.

use std::collections::HashMap;

use sqlx::{Connection, Row, SqliteConnection};
use uuid::Uuid;

use super::{Store, StoreResult};

const HEARTBEAT_EVERY_SECS: i64 = 10;
const READER_STALE_SECS: i64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeOrigin {
    Local,
    OtherProcess,
    Server,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocChange {
    pub doc_id: Uuid,
    pub deleted: bool,
    pub origin: ChangeOrigin,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LogRead {
    /// One entry per document, in order of first appearance, carrying its latest row.
    Docs(Vec<DocChange>),
    /// Rows this reader had not read were trimmed; the host should reload its lists.
    DatabaseChanged,
}

pub struct ChangeLogReader {
    conn: SqliteConnection,
    instance_id: Uuid,
    position: i64,
    data_version: i64,
    heartbeat_at: i64,
}

impl ChangeLogReader {
    /// Starts at the head: changes made before the engine started are not replayed.
    pub async fn open(store: &Store, now_unix: i64) -> StoreResult<ChangeLogReader> {
        let mut conn = SqliteConnection::connect_with(&store.options).await?;
        let position = head(&mut conn).await?;
        let data_version = data_version(&mut conn).await?;
        let mut reader = ChangeLogReader {
            conn,
            instance_id: store.instance_id(),
            position,
            data_version,
            heartbeat_at: now_unix,
        };
        reader.save(now_unix).await?;
        Ok(reader)
    }

    /// Call on every driver tick. Cheap when nothing changed: it checks `PRAGMA data_version`
    /// first and only opens a transaction when another connection has committed since the
    /// last call, but it still heartbeats on schedule either way, so an idle reader is never
    /// trimmed past.
    pub async fn read(&mut self, now_unix: i64) -> StoreResult<LogRead> {
        let version = data_version(&mut self.conn).await?;
        if version == self.data_version {
            if now_unix - self.heartbeat_at >= HEARTBEAT_EVERY_SECS {
                self.save(now_unix).await?;
            }
            return Ok(LogRead::Docs(Vec::new()));
        }
        // `self.data_version`/`self.position` are advanced only once the whole read has
        // fully succeeded (just before returning): if a decode fails partway, or the
        // transaction fails to commit, the reader must retry the same rows next time
        // instead of silently skipping them.

        // Deferred transaction: the head, the trim floor and the rows are one consistent
        // snapshot, so a concurrent trim between these selects cannot make us miss rows
        // that were still present when we computed `trimmed_through`.
        let mut tx = self.conn.begin().await?;
        let head = head(&mut tx).await?;
        let lowest: Option<i64> = sqlx::query_scalar("SELECT MIN(local_seq) FROM change_log")
            .fetch_one(&mut *tx)
            .await?;
        // Trimming deletes a prefix, so everything below the lowest row (or the whole log
        // when it is empty) is gone.
        let trimmed_through = lowest.map_or(head, |lowest| lowest - 1);
        if self.position < trimmed_through {
            tx.commit().await?;
            self.position = head;
            self.data_version = version;
            self.save(now_unix).await?;
            return Ok(LogRead::DatabaseChanged);
        }

        let rows = sqlx::query(
            "SELECT local_seq, doc_id, kind, origin_instance, origin FROM change_log \
             WHERE local_seq > ? ORDER BY local_seq",
        )
        .bind(self.position)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        let me = self.instance_id.to_string();
        let mut order = Vec::new();
        let mut latest: HashMap<Uuid, DocChange> = HashMap::new();
        let mut position = self.position;
        for row in &rows {
            position = row.try_get("local_seq")?;
            let doc_id = Uuid::parse_str(&row.try_get::<String, _>("doc_id")?)?;
            let origin = match row.try_get::<String, _>("origin")?.as_str() {
                "server" => ChangeOrigin::Server,
                _ if row.try_get::<String, _>("origin_instance")? == me => ChangeOrigin::Local,
                _ => ChangeOrigin::OtherProcess,
            };
            let change = DocChange {
                doc_id,
                deleted: row.try_get::<String, _>("kind")? == "delete",
                origin,
            };
            if latest.insert(doc_id, change).is_none() {
                order.push(doc_id);
            }
        }
        self.position = position;
        self.data_version = version;
        if now_unix - self.heartbeat_at >= HEARTBEAT_EVERY_SECS {
            self.save(now_unix).await?;
        }
        Ok(LogRead::Docs(
            order
                .into_iter()
                .filter_map(|doc_id| latest.remove(&doc_id))
                .collect(),
        ))
    }

    /// Removes this reader so it never holds back trimming, then closes its connection.
    pub async fn close(mut self) -> StoreResult<()> {
        sqlx::query("DELETE FROM change_log_readers WHERE instance_id = ?")
            .bind(self.instance_id.to_string())
            .execute(&mut self.conn)
            .await?;
        self.conn.close().await?;
        Ok(())
    }

    /// Persists position and heartbeat, drops reader rows that have gone stale (a crashed
    /// instance must not hold back trimming forever), then trims change-log rows at or below
    /// the lowest position among the readers that are left.
    async fn save(&mut self, now_unix: i64) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO change_log_readers (instance_id, position, heartbeat_at) VALUES (?, ?, ?) \
             ON CONFLICT(instance_id) DO UPDATE SET position = excluded.position, \
             heartbeat_at = excluded.heartbeat_at",
        )
        .bind(self.instance_id.to_string())
        .bind(self.position)
        .bind(now_unix)
        .execute(&mut self.conn)
        .await?;
        self.heartbeat_at = now_unix;
        let stale_before = now_unix - READER_STALE_SECS;
        sqlx::query("DELETE FROM change_log_readers WHERE heartbeat_at < ?")
            .bind(stale_before)
            .execute(&mut self.conn)
            .await?;
        sqlx::query(
            "DELETE FROM change_log WHERE local_seq <= \
             (SELECT MIN(position) FROM change_log_readers WHERE heartbeat_at >= ?)",
        )
        .bind(stale_before)
        .execute(&mut self.conn)
        .await?;
        Ok(())
    }
}

/// The highest `local_seq` ever assigned (AUTOINCREMENT never reuses one).
async fn head(conn: &mut SqliteConnection) -> StoreResult<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'change_log'), 0)",
    )
    .fetch_one(&mut *conn)
    .await?)
}

async fn data_version(conn: &mut SqliteConnection) -> StoreResult<i64> {
    Ok(sqlx::query_scalar("PRAGMA data_version")
        .fetch_one(&mut *conn)
        .await?)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::engine::types::SCOPE_OWN;
    use crate::store::test_support::*;

    const T0: i64 = 1_000_000;

    #[tokio::test]
    async fn writes_are_reported_with_their_origin() {
        let t = temp_store().await;
        let other = open_again(&t.path()).await;
        let mut reader = ChangeLogReader::open(&t.store, T0).await.unwrap();
        let mine = t
            .store
            .create_document(ME, None, json!({"a": 1}))
            .await
            .unwrap();
        let theirs = other
            .create_document(ME, None, json!({"b": 1}))
            .await
            .unwrap();
        let from_server = Uuid::from_u128(0xD1);
        let push = upsert_change(
            SCOPE_OWN,
            envelope(from_server, Some(ME), json!({"c": 1}), 5),
        );
        t.store
            .apply_changes(ME, SCOPE_OWN, &[push], 5)
            .await
            .unwrap();

        assert_eq!(
            reader.read(T0).await.unwrap(),
            LogRead::Docs(vec![
                DocChange {
                    doc_id: mine,
                    deleted: false,
                    origin: ChangeOrigin::Local
                },
                DocChange {
                    doc_id: theirs,
                    deleted: false,
                    origin: ChangeOrigin::OtherProcess
                },
                DocChange {
                    doc_id: from_server,
                    deleted: false,
                    origin: ChangeOrigin::Server
                },
            ])
        );
        assert_eq!(reader.read(T0).await.unwrap(), LogRead::Docs(vec![]));
    }

    #[tokio::test]
    async fn several_rows_for_one_doc_merge_into_one_change() {
        let t = temp_store().await;
        let mut reader = ChangeLogReader::open(&t.store, T0).await.unwrap();
        let doc_id = t
            .store
            .create_document(ME, None, json!({"n": 0}))
            .await
            .unwrap();
        t.store
            .update_document(ME, doc_id, json!({"n": 1}))
            .await
            .unwrap();
        t.store
            .update_document(ME, doc_id, json!({"n": 2}))
            .await
            .unwrap();
        t.store.delete_document(ME, doc_id).await.unwrap();
        assert_eq!(
            reader.read(T0).await.unwrap(),
            LogRead::Docs(vec![DocChange {
                doc_id,
                deleted: true,
                origin: ChangeOrigin::Local
            }])
        );
    }

    /// SQLite's `data_version` does not change for writes made by the same connection that
    /// reads it, so a row inserted through the reader's own connection proves the version
    /// looked unchanged and `read` skipped the transaction that would otherwise have seen it.
    #[tokio::test]
    async fn unchanged_data_version_skips_the_read_transaction() {
        let t = temp_store().await;
        let mut reader = ChangeLogReader::open(&t.store, T0).await.unwrap();
        sqlx::query(
            "INSERT INTO change_log (doc_id, kind, origin_instance, origin) \
             VALUES (?, 'upsert', ?, 'local')",
        )
        .bind(Uuid::from_u128(0xF00D).to_string())
        .bind(reader.instance_id.to_string())
        .execute(&mut reader.conn)
        .await
        .unwrap();

        assert_eq!(reader.read(T0).await.unwrap(), LogRead::Docs(vec![]));
    }

    /// A read that fails partway through decoding must not advance `data_version` or
    /// `position`, so the same rows are retried (not silently skipped) once whatever made
    /// decoding fail is fixed.
    #[tokio::test]
    async fn a_read_that_fails_partway_leaves_position_and_version_unchanged() {
        let t = temp_store().await;
        let mut reader = ChangeLogReader::open(&t.store, T0).await.unwrap();
        let good_id = t
            .store
            .create_document(ME, None, json!({"a": 1}))
            .await
            .unwrap();
        // An undecodable row after the good one: `read` must fail without losing `good_id`.
        sqlx::query(
            "INSERT INTO change_log (doc_id, kind, origin_instance, origin) \
             VALUES ('not-a-uuid', 'upsert', ?, 'local')",
        )
        .bind(Uuid::new_v4().to_string())
        .execute(&t.store.pool)
        .await
        .unwrap();

        assert!(reader.read(T0).await.is_err());

        // Repair via the reader's own connection: `data_version` does not count a
        // connection's own writes, so this does not mask whether the failed `read` had
        // already (wrongly) advanced past the version that made the bad row visible.
        sqlx::query("DELETE FROM change_log WHERE doc_id = 'not-a-uuid'")
            .execute(&mut reader.conn)
            .await
            .unwrap();

        assert_eq!(
            reader.read(T0).await.unwrap(),
            LogRead::Docs(vec![DocChange {
                doc_id: good_id,
                deleted: false,
                origin: ChangeOrigin::Local,
            }])
        );
    }

    /// A commit landing between two `read` calls must never be swallowed by the version check.
    #[tokio::test]
    async fn version_change_between_reads_is_never_missed() {
        let t = temp_store().await;
        let other = open_again(&t.path()).await;
        let mut reader = ChangeLogReader::open(&t.store, T0).await.unwrap();
        assert_eq!(reader.read(T0).await.unwrap(), LogRead::Docs(vec![]));

        let doc_id = other
            .create_document(ME, None, json!({"x": 1}))
            .await
            .unwrap();
        assert_eq!(
            reader.read(T0).await.unwrap(),
            LogRead::Docs(vec![DocChange {
                doc_id,
                deleted: false,
                origin: ChangeOrigin::OtherProcess,
            }])
        );
    }

    #[tokio::test]
    async fn reader_behind_the_trim_point_gets_database_changed_once() {
        let t = temp_store().await;
        let other = open_again(&t.path()).await;
        let mut suspended = ChangeLogReader::open(&t.store, T0).await.unwrap();
        let mut live = ChangeLogReader::open(&other, T0).await.unwrap();
        t.store
            .create_document(ME, None, json!({"a": 1}))
            .await
            .unwrap();

        // 100 s later only `live` has a fresh heartbeat, so its read trims past `suspended`.
        live.read(T0 + 100).await.unwrap();
        assert_eq!(count(&t.store, "SELECT COUNT(*) FROM change_log").await, 0);

        assert_eq!(
            suspended.read(T0 + 101).await.unwrap(),
            LogRead::DatabaseChanged
        );
        assert_eq!(
            suspended.read(T0 + 102).await.unwrap(),
            LogRead::Docs(vec![])
        );
    }

    #[tokio::test]
    async fn closed_reader_no_longer_holds_back_trim() {
        let t = temp_store().await;
        let other = open_again(&t.path()).await;
        let closing = ChangeLogReader::open(&t.store, T0).await.unwrap();
        let mut live = ChangeLogReader::open(&other, T0).await.unwrap();
        t.store.create_document(ME, None, json!({})).await.unwrap();
        closing.close().await.unwrap();

        live.read(T0 + 10).await.unwrap();

        assert_eq!(count(&t.store, "SELECT COUNT(*) FROM change_log").await, 0);
    }

    #[tokio::test]
    async fn sustained_activity_still_trims_old_rows() {
        let t = temp_store().await;
        let mut reader = ChangeLogReader::open(&t.store, T0).await.unwrap();
        for i in 1..=10 {
            t.store
                .create_document(ME, None, json!({"i": i}))
                .await
                .unwrap();
            reader.read(T0 + i).await.unwrap();
        }
        assert_eq!(count(&t.store, "SELECT COUNT(*) FROM change_log").await, 0);
    }

    /// A reader driven only through `read` (the real driver contract: one tick, no other
    /// method) with no local writes of its own must still heartbeat often enough that another
    /// process's trim, run well past the 60 s stale window, never passes it.
    #[tokio::test]
    async fn idle_reader_driven_by_read_alone_stays_fresh_past_the_stale_window() {
        let t = temp_store().await;
        let other = open_again(&t.path()).await;
        let mut idle = ChangeLogReader::open(&t.store, T0).await.unwrap();
        let mut active = ChangeLogReader::open(&other, T0).await.unwrap();

        let mut now = T0;
        for _tick in 0..65 {
            now += 1;
            assert_eq!(idle.read(now).await.unwrap(), LogRead::Docs(vec![]));
        }

        let doc_id = other
            .create_document(ME, None, json!({"a": 1}))
            .await
            .unwrap();
        active.read(now + 1).await.unwrap();

        assert_eq!(
            idle.read(now + 2).await.unwrap(),
            LogRead::Docs(vec![DocChange {
                doc_id,
                deleted: false,
                origin: ChangeOrigin::OtherProcess,
            }])
        );
    }
}
