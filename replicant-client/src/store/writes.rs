//! Local writes: document, outbox marker and change-log row in one transaction.

use serde_json::Value;
use sqlx::SqliteConnection;
use uuid::Uuid;

use super::docs::{
    append_change_log, insert_marker, load_snapshot, refresh_search, title_of, LogOrigin,
};
use super::{now_rfc3339, Store, StoreError, StoreResult};
use crate::engine::doc::{DocSnapshot, RowKind};
use crate::engine::hash::content_hash;

impl Store {
    /// Creates a document owned by `me`, returning its id.
    pub async fn create_document(
        &self,
        me: Uuid,
        doc_id: Option<Uuid>,
        content: Value,
    ) -> StoreResult<Uuid> {
        let doc_id = doc_id.unwrap_or_else(Uuid::new_v4);
        let mut tx = self.begin().await?;
        let snap = load_snapshot(&mut tx, doc_id).await?;
        if snap.exists || snap.tombstone_seq.is_some() {
            return Err(StoreError::AlreadyExists(doc_id));
        }
        let now = now_rfc3339();
        sqlx::query(
            "INSERT INTO documents (id, user_id, content, hash, title, read_only, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, 0, ?, ?)",
        )
        .bind(doc_id.to_string())
        .bind(me.to_string())
        .bind(content.to_string())
        .bind(content_hash(&content))
        .bind(title_of(&content, None))
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        insert_marker(&mut tx, doc_id, RowKind::Create).await?;
        append_change_log(&mut tx, &self.writer(LogOrigin::Local), doc_id, false).await?;
        refresh_search(&mut tx, doc_id).await?;
        tx.commit().await?;
        Ok(doc_id)
    }

    pub async fn update_document(&self, me: Uuid, doc_id: Uuid, content: Value) -> StoreResult<()> {
        let mut tx = self.begin().await?;
        let snap = load_snapshot(&mut tx, doc_id).await?;
        check_writable(&snap, me)?;
        sqlx::query(
            "UPDATE documents SET content = ?, hash = ?, title = COALESCE(?, title), updated_at = ? \
             WHERE id = ?",
        )
        .bind(content.to_string())
        .bind(content_hash(&content))
        .bind(title_of(&content, None))
        .bind(now_rfc3339())
        .bind(doc_id.to_string())
        .execute(&mut *tx)
        .await?;
        unpark(&mut tx, doc_id).await?;
        insert_marker(&mut tx, doc_id, RowKind::Update).await?;
        append_change_log(&mut tx, &self.writer(LogOrigin::Local), doc_id, false).await?;
        refresh_search(&mut tx, doc_id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Soft delete: the shadow stays so the delete upload can be built.
    pub async fn delete_document(&self, me: Uuid, doc_id: Uuid) -> StoreResult<()> {
        let mut tx = self.begin().await?;
        let snap = load_snapshot(&mut tx, doc_id).await?;
        check_writable(&snap, me)?;
        let now = now_rfc3339();
        sqlx::query("UPDATE documents SET deleted_at = ?, updated_at = ? WHERE id = ?")
            .bind(&now)
            .bind(&now)
            .bind(doc_id.to_string())
            .execute(&mut *tx)
            .await?;
        unpark(&mut tx, doc_id).await?;
        insert_marker(&mut tx, doc_id, RowKind::Delete).await?;
        append_change_log(&mut tx, &self.writer(LogOrigin::Local), doc_id, true).await?;
        refresh_search(&mut tx, doc_id).await?;
        tx.commit().await?;
        Ok(())
    }
}

fn check_writable(snap: &DocSnapshot, me: Uuid) -> StoreResult<()> {
    if !snap.exists || snap.soft_deleted {
        return Err(StoreError::NotFound(snap.doc_id));
    }
    if !snap.writable(me) {
        return Err(StoreError::NotWritable(snap.doc_id));
    }
    Ok(())
}

/// A new local edit may fix what the server refused, so parked rows are retried.
async fn unpark(conn: &mut SqliteConnection, doc_id: Uuid) -> StoreResult<()> {
    sqlx::query("UPDATE outbox SET parked_error = NULL WHERE doc_id = ?")
        .bind(doc_id.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::database::ClientDatabase;
    use crate::engine::doc::DocOp;
    use crate::engine::types::{SCOPE_CURATED, SCOPE_OWN};
    use crate::store::test_support::*;

    const OTHER: Uuid = Uuid::from_u128(0xB);

    fn doc(n: u128) -> Uuid {
        Uuid::from_u128(0xD000 + n)
    }

    #[tokio::test]
    async fn create_writes_document_marker_and_log_row_together() {
        let t = temp_store().await;
        let doc_id = t
            .store
            .create_document(ME, None, json!({"title": "New"}))
            .await
            .unwrap();
        let snap = snapshot(&t.store, doc_id).await;
        assert!(snap.exists);
        assert_eq!(snap.owner_id, Some(ME));
        assert_eq!(snap.content, json!({"title": "New"}));
        assert!(snap.shadow.is_none());
        assert_eq!(snap.rows.len(), 1);
        assert_eq!(snap.rows[0].kind, RowKind::Create);
        let log: (String, String, String) =
            sqlx::query_as("SELECT kind, origin, origin_instance FROM change_log")
                .fetch_one(&t.store.pool)
                .await
                .unwrap();
        assert_eq!(
            log,
            (
                "upsert".to_string(),
                "local".to_string(),
                t.store.instance_id().to_string()
            )
        );
    }

    #[tokio::test]
    async fn create_rejects_existing_soft_deleted_or_tombstoned_ids() {
        let t = temp_store().await;
        t.store
            .create_document(ME, Some(doc(1)), json!({}))
            .await
            .unwrap();
        let again = t.store.create_document(ME, Some(doc(1)), json!({})).await;
        assert!(matches!(again, Err(StoreError::AlreadyExists(id)) if id == doc(1)));

        t.store.delete_document(ME, doc(1)).await.unwrap();
        let after_delete = t.store.create_document(ME, Some(doc(1)), json!({})).await;
        assert!(matches!(after_delete, Err(StoreError::AlreadyExists(_))));

        apply(&t.store, doc(2), |_| vec![DocOp::RecordTombstone(5)]).await;
        let tombstoned = t.store.create_document(ME, Some(doc(2)), json!({})).await;
        assert!(matches!(tombstoned, Err(StoreError::AlreadyExists(id)) if id == doc(2)));
    }

    #[tokio::test]
    async fn update_and_delete_reject_foreign_read_only_and_missing_documents() {
        let t = temp_store().await;
        seed_synced(&t.store, doc(1), SCOPE_OWN, Some(OTHER), json!({}), 1).await;
        let foreign = t.store.update_document(ME, doc(1), json!({"x": 1})).await;
        assert!(matches!(foreign, Err(StoreError::NotWritable(id)) if id == doc(1)));

        seed_synced(&t.store, doc(2), SCOPE_CURATED, Some(ME), json!({}), 1).await;
        apply(&t.store, doc(2), |_| {
            vec![DocOp::SetMeta {
                owner_id: Some(ME),
                read_only: true,
            }]
        })
        .await;
        assert!(matches!(
            t.store.update_document(ME, doc(2), json!({"x": 1})).await,
            Err(StoreError::NotWritable(_))
        ));
        assert!(matches!(
            t.store.delete_document(ME, doc(2)).await,
            Err(StoreError::NotWritable(_))
        ));

        assert!(matches!(
            t.store.update_document(ME, doc(3), json!({})).await,
            Err(StoreError::NotFound(id)) if id == doc(3)
        ));
        let created = t.store.create_document(ME, None, json!({})).await.unwrap();
        t.store.delete_document(ME, created).await.unwrap();
        assert!(matches!(
            t.store.delete_document(ME, created).await,
            Err(StoreError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn new_edit_unparks_rows() {
        let t = temp_store().await;
        seed_synced(&t.store, doc(1), SCOPE_OWN, Some(ME), json!({"n": 0}), 1).await;
        t.store
            .update_document(ME, doc(1), json!({"n": 1}))
            .await
            .unwrap();
        apply(&t.store, doc(1), |snap| {
            vec![DocOp::Park {
                rows: snap.rows.iter().map(|r| r.mutation_id).collect(),
                error: "validation".into(),
            }]
        })
        .await;
        assert!(snapshot(&t.store, doc(1)).await.rows[0].parked);

        t.store
            .update_document(ME, doc(1), json!({"n": 2}))
            .await
            .unwrap();
        let rows = snapshot(&t.store, doc(1)).await.rows;
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| !r.parked));
    }

    /// A mutation_id far ahead of any real `now_v7()` output for a long time, so a planted
    /// row simulates a clock step or a race from another process.
    fn far_future_marker() -> Uuid {
        Uuid::from_u128(Uuid::now_v7().as_u128() + (1u128 << 96))
    }

    async fn plant_marker(store: &Store, doc_id: Uuid, mutation_id: Uuid) {
        sqlx::query(
            "INSERT INTO outbox (mutation_id, doc_id, kind, created_at) VALUES (?, ?, 'update', 0)",
        )
        .bind(mutation_id.to_string())
        .bind(doc_id.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn new_marker_stays_ahead_of_a_planted_future_mutation_id() {
        let t = temp_store().await;
        seed_synced(&t.store, doc(1), SCOPE_OWN, Some(ME), json!({"n": 0}), 1).await;
        let planted = far_future_marker();
        plant_marker(&t.store, doc(1), planted).await;

        t.store
            .update_document(ME, doc(1), json!({"n": 1}))
            .await
            .unwrap();

        let rows = snapshot(&t.store, doc(1)).await.rows;
        let newest = rows.last().unwrap();
        assert!(newest.mutation_id > planted);
        assert_eq!(newest.kind, RowKind::Update);

        t.store.delete_document(ME, doc(1)).await.unwrap();
        let rows = snapshot(&t.store, doc(1)).await.rows;
        let newest = rows.last().unwrap();
        assert!(newest.mutation_id > planted);
        assert_eq!(newest.kind, RowKind::Delete);
    }

    #[tokio::test]
    async fn a_second_store_also_stays_ahead_of_a_planted_future_mutation_id() {
        let t = temp_store().await;
        seed_synced(&t.store, doc(1), SCOPE_OWN, Some(ME), json!({"n": 0}), 1).await;
        let planted = far_future_marker();
        plant_marker(&t.store, doc(1), planted).await;

        let other = open_again(&t.path()).await;
        other
            .update_document(ME, doc(1), json!({"n": 1}))
            .await
            .unwrap();

        let rows = snapshot(&t.store, doc(1)).await.rows;
        let newest = rows.last().unwrap();
        assert!(newest.mutation_id > planted);
        assert_eq!(newest.kind, RowKind::Update);
    }

    #[tokio::test]
    async fn delete_is_soft_and_keeps_the_shadow() {
        let t = temp_store().await;
        seed_synced(&t.store, doc(1), SCOPE_OWN, Some(ME), json!({"n": 1}), 3).await;
        t.store.delete_document(ME, doc(1)).await.unwrap();
        let snap = snapshot(&t.store, doc(1)).await;
        assert!(snap.exists);
        assert!(snap.soft_deleted);
        assert_eq!(snap.shadow.as_ref().unwrap().seq, 3);
        assert_eq!(snap.rows.last().unwrap().kind, RowKind::Delete);
        let last: String =
            sqlx::query_scalar("SELECT kind FROM change_log ORDER BY local_seq DESC LIMIT 1")
                .fetch_one(&t.store.pool)
                .await
                .unwrap();
        assert_eq!(last, "delete");
    }

    #[tokio::test]
    async fn v1_reader_parses_v2_written_documents() {
        let t = temp_store().await;
        let doc_id = t
            .store
            .create_document(ME, None, json!({"title": "From v2"}))
            .await
            .unwrap();
        let v1 = ClientDatabase::new(&format!("sqlite://{}?mode=rwc", t.path().display()))
            .await
            .unwrap();
        let read = v1.get_document(&doc_id).await.unwrap();
        assert_eq!(read.content, json!({"title": "From v2"}));
        assert_eq!(read.title.as_deref(), Some("From v2"));
        t.store.delete_document(ME, doc_id).await.unwrap();
        assert!(v1.get_document(&doc_id).await.unwrap().deleted_at.is_some());
        v1.close().await;
    }

    #[tokio::test]
    async fn rapid_edits_keep_marker_order() {
        let t = temp_store().await;
        let doc_id = t
            .store
            .create_document(ME, None, json!({"n": 0}))
            .await
            .unwrap();
        for n in 1..=200 {
            t.store
                .update_document(ME, doc_id, json!({"n": n}))
                .await
                .unwrap();
        }
        let by_insertion: Vec<String> =
            sqlx::query_scalar("SELECT mutation_id FROM outbox WHERE doc_id = ? ORDER BY rowid")
                .bind(doc_id.to_string())
                .fetch_all(&t.store.pool)
                .await
                .unwrap();
        let by_id: Vec<String> = snapshot(&t.store, doc_id)
            .await
            .rows
            .iter()
            .map(|r| r.mutation_id.to_string())
            .collect();
        assert_eq!(by_insertion.len(), 201);
        assert_eq!(by_insertion, by_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_stores_on_one_file_write_concurrently() {
        let t = temp_store().await;
        let other = open_again(&t.path()).await;
        let doc_id = t
            .store
            .create_document(ME, None, json!({"n": 0}))
            .await
            .unwrap();
        let mine = async {
            for n in 0..50 {
                t.store
                    .update_document(ME, doc_id, json!({"a": n}))
                    .await
                    .unwrap();
            }
        };
        let theirs = async {
            for n in 0..50 {
                other
                    .update_document(ME, doc_id, json!({"b": n}))
                    .await
                    .unwrap();
            }
        };
        tokio::join!(mine, theirs);
        assert_eq!(count(&t.store, "SELECT COUNT(*) FROM outbox").await, 101);
    }

    #[tokio::test]
    async fn open_write_close_cycles_keep_every_write() {
        let t = temp_store().await;
        t.store.close().await;
        for n in 0..20 {
            let store = open_again(&t.path()).await;
            store
                .create_document(ME, None, json!({"n": n}))
                .await
                .unwrap();
            store.close().await;
        }
        let reopened = open_again(&t.path()).await;
        assert_eq!(count(&reopened, "SELECT COUNT(*) FROM documents").await, 20);
    }
}
