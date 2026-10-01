//! Local writes: document, outbox marker and change-log row in one transaction.

use serde_json::Value;
use sqlx::SqliteConnection;
use uuid::Uuid;

use super::docs::{
    append_change_log, insert_marker, load_snapshot, refresh_search, title_of, LogOrigin,
};
use super::{now_rfc3339, read_user_id, Store, StoreError, StoreResult};
use crate::engine::doc::{DocSnapshot, FieldConflict, RowKind};
use crate::engine::hash::{canonicalise_numbers, content_hash};

impl Store {
    /// Creates a document owned by this data dir's user, returning its id.
    pub async fn create_document(&self, doc_id: Option<Uuid>, content: Value) -> StoreResult<Uuid> {
        let doc_id = doc_id.unwrap_or_else(Uuid::new_v4);
        let mut tx = self.begin().await?;
        self.create_in(&mut tx, doc_id, content).await?;
        tx.commit().await?;
        Ok(doc_id)
    }

    async fn create_in(
        &self,
        conn: &mut SqliteConnection,
        doc_id: Uuid,
        mut content: Value,
    ) -> StoreResult<()> {
        canonicalise_numbers(&mut content);
        let content = &content;
        let snap = load_snapshot(&mut *conn, doc_id).await?;
        if snap.exists || snap.tombstone_seq.is_some() {
            return Err(StoreError::AlreadyExists(doc_id));
        }
        let owner = read_user_id(&mut *conn).await?;
        let now = now_rfc3339();
        sqlx::query(
            "INSERT INTO documents (id, user_id, content, hash, title, read_only, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, 0, ?, ?)",
        )
        .bind(doc_id.to_string())
        .bind(owner.to_string())
        .bind(content.to_string())
        .bind(content_hash(content))
        .bind(title_of(content, self.title_pointer.as_deref()))
        .bind(&now)
        .bind(&now)
        .execute(&mut *conn)
        .await?;
        insert_marker(&mut *conn, doc_id, RowKind::Create).await?;
        append_change_log(&mut *conn, &self.writer(LogOrigin::Local), doc_id, false).await?;
        refresh_search(&mut *conn, doc_id).await?;
        Ok(())
    }

    pub async fn update_document(&self, doc_id: Uuid, content: Value) -> StoreResult<()> {
        let mut tx = self.begin().await?;
        self.update_in(&mut tx, doc_id, content).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Writes a field conflict's kept local values back at their paths as a local edit and
    /// deletes the kept copy, in one transaction. Other paths keep the server's values.
    pub async fn restore_fields(&self, recovered_id: i64) -> StoreResult<()> {
        let mut tx = self.begin().await?;
        let kept: Option<(String, Option<String>, String)> =
            sqlx::query_as("SELECT doc_id, fields, reason FROM recovered WHERE id = ?")
                .bind(recovered_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((doc_id, fields, reason)) = kept else {
            return Err(StoreError::NoKeptCopy(recovered_id));
        };
        let (Some(fields), "field_conflict") = (fields, reason.as_str()) else {
            return Err(StoreError::NoFieldConflict(recovered_id));
        };
        let doc_id = Uuid::parse_str(&doc_id)?;
        let fields: Vec<FieldConflict> = serde_json::from_str(&fields)?;
        // Checked before put_field: a deleted document would otherwise report Corrupt.
        let snap = load_snapshot(&mut tx, doc_id).await?;
        if !snap.exists || snap.soft_deleted {
            return Err(StoreError::DocumentGone(doc_id));
        }
        let mut content = snap.content;
        for field in &fields {
            put_field(&mut content, field)?;
        }
        self.update_in(&mut tx, doc_id, content).await?;
        sqlx::query("DELETE FROM recovered WHERE id = ?")
            .bind(recovered_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Re-creates a kept copy's full content as a new document (its old id may be tombstoned on
    /// the server) and deletes the copy, in one transaction. A field copy holds the whole local
    /// document too, so this is also the way out when its document is gone or read-only.
    pub async fn restore_document(&self, recovered_id: i64) -> StoreResult<Uuid> {
        let mut tx = self.begin().await?;
        let content: Option<String> =
            sqlx::query_scalar("SELECT content FROM recovered WHERE id = ?")
                .bind(recovered_id)
                .fetch_optional(&mut *tx)
                .await?;
        let content: Value =
            serde_json::from_str(&content.ok_or(StoreError::NoKeptCopy(recovered_id))?)?;
        let doc_id = Uuid::new_v4();
        self.create_in(&mut tx, doc_id, content).await?;
        sqlx::query("DELETE FROM recovered WHERE id = ?")
            .bind(recovered_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(doc_id)
    }

    async fn update_in(
        &self,
        conn: &mut SqliteConnection,
        doc_id: Uuid,
        mut content: Value,
    ) -> StoreResult<()> {
        canonicalise_numbers(&mut content);
        let snap = load_snapshot(&mut *conn, doc_id).await?;
        check_writable(&snap, read_user_id(&mut *conn).await?)?;
        sqlx::query(
            "UPDATE documents SET content = ?, hash = ?, title = ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(content.to_string())
        .bind(content_hash(&content))
        .bind(title_of(&content, self.title_pointer.as_deref()))
        .bind(now_rfc3339())
        .bind(doc_id.to_string())
        .execute(&mut *conn)
        .await?;
        unpark(&mut *conn, doc_id).await?;
        insert_marker(&mut *conn, doc_id, RowKind::Update).await?;
        append_change_log(&mut *conn, &self.writer(LogOrigin::Local), doc_id, false).await?;
        refresh_search(&mut *conn, doc_id).await?;
        Ok(())
    }

    /// Soft delete: the shadow stays so the delete upload can be built.
    pub async fn delete_document(&self, doc_id: Uuid) -> StoreResult<()> {
        let mut tx = self.begin().await?;
        let snap = load_snapshot(&mut tx, doc_id).await?;
        check_writable(&snap, read_user_id(&mut tx).await?)?;
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

/// Sets `field.path` to the kept local value (removes it where the local side had removed it),
/// creating missing object parents; an array index past the end appends.
fn put_field(content: &mut Value, field: &FieldConflict) -> StoreResult<()> {
    let unrestorable = || StoreError::Corrupt(format!("cannot restore {} here", field.path));
    let tokens: Vec<String> = field
        .path
        .split('/')
        .skip(1)
        .map(|token| token.replace("~1", "/").replace("~0", "~"))
        .collect();
    let Some((last, parents)) = tokens.split_last() else {
        return Err(unrestorable());
    };
    let mut target = content;
    for token in parents {
        target = match target {
            Value::Object(fields) => fields
                .entry(token.clone())
                .or_insert_with(|| Value::Object(Default::default())),
            Value::Array(items) => {
                let index: usize = token.parse().map_err(|_| unrestorable())?;
                items.get_mut(index).ok_or_else(unrestorable)?
            }
            _ => return Err(unrestorable()),
        };
    }
    match target {
        Value::Object(fields) if field.local_removed => {
            fields.remove(last);
        }
        Value::Object(fields) => {
            fields.insert(last.clone(), field.local_value.clone());
        }
        Value::Array(items) => {
            let index: usize = last.parse().map_err(|_| unrestorable())?;
            if field.local_removed {
                if index < items.len() {
                    items.remove(index);
                }
            } else if index < items.len() {
                items[index] = field.local_value.clone();
            } else {
                items.push(field.local_value.clone());
            }
        }
        _ => return Err(unrestorable()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use uuid::Uuid;

    use super::*;
    use crate::engine::doc::DocOp;
    use crate::engine::types::{SCOPE_CURATED, SCOPE_OWN};
    use crate::store::test_support::*;

    const OTHER: Uuid = Uuid::from_u128(0xB);

    #[tokio::test]
    async fn a_write_after_another_process_adopts_the_identity_is_stamped_with_the_adopted_id() {
        let t = temp_store().await;
        let provisional = Uuid::from_u128(0x1);
        let server = Uuid::from_u128(0x2);
        seed_user(&t.store, provisional, false).await;
        let before = t
            .store
            .create_document(None, json!({"n": 1}))
            .await
            .unwrap();
        let other_process = open_again(&t.path()).await;
        other_process.check_identity(server).await.unwrap();
        let after = t
            .store
            .create_document(None, json!({"n": 2}))
            .await
            .unwrap();
        assert_eq!(
            snapshot(&t.store, before).await.owner_id,
            Some(server),
            "re-stamped by the other process's join"
        );
        assert_eq!(
            snapshot(&t.store, after).await.owner_id,
            Some(server),
            "stamped with the adopted id, not the one this process saw first"
        );
        t.store
            .update_document(after, json!({"n": 3}))
            .await
            .unwrap();
        t.store.delete_document(before).await.unwrap();
        other_process.close().await;
    }

    fn doc(n: u128) -> Uuid {
        Uuid::from_u128(0xD000 + n)
    }

    #[tokio::test]
    async fn host_writes_store_integral_floats_as_integers() {
        let t = temp_store().await;
        let doc_id = t
            .store
            .create_document(None, json!({"n": 1200.0, "x": 1.5, "list": [2.0]}))
            .await
            .unwrap();
        let canonical = json!({"n": 1200, "x": 1.5, "list": [2]});
        assert_eq!(snapshot(&t.store, doc_id).await.content, canonical);
        let hash: String = sqlx::query_scalar("SELECT hash FROM documents WHERE id = ?")
            .bind(doc_id.to_string())
            .fetch_one(&t.store.pool)
            .await
            .unwrap();
        assert_eq!(hash, content_hash(&canonical));
        t.store
            .update_document(doc_id, json!({"n": 1300.0}))
            .await
            .unwrap();
        assert_eq!(snapshot(&t.store, doc_id).await.content, json!({"n": 1300}));
    }

    #[tokio::test]
    async fn a_local_edit_sets_the_title_from_the_pointer_and_clears_it_when_the_value_goes() {
        let mut t = temp_store().await;
        t.store.title_pointer = Some("/name".into());
        let doc_id = t
            .store
            .create_document(None, json!({"name": "Scale"}))
            .await
            .unwrap();
        let title = |store: &Store| {
            let pool = store.pool.clone();
            async move {
                sqlx::query_scalar::<_, Option<String>>("SELECT title FROM documents WHERE id = ?")
                    .bind(doc_id.to_string())
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(title(&t.store).await.as_deref(), Some("Scale"));
        t.store
            .update_document(doc_id, json!({"name": 7}))
            .await
            .unwrap();
        assert_eq!(title(&t.store).await, None);
    }

    #[tokio::test]
    async fn create_writes_document_marker_and_log_row_together() {
        let t = temp_store().await;
        let doc_id = t
            .store
            .create_document(None, json!({"title": "New"}))
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
            .create_document(Some(doc(1)), json!({}))
            .await
            .unwrap();
        let again = t.store.create_document(Some(doc(1)), json!({})).await;
        assert!(matches!(again, Err(StoreError::AlreadyExists(id)) if id == doc(1)));

        t.store.delete_document(doc(1)).await.unwrap();
        let after_delete = t.store.create_document(Some(doc(1)), json!({})).await;
        assert!(matches!(after_delete, Err(StoreError::AlreadyExists(_))));

        apply(&t.store, doc(2), |_| vec![DocOp::RecordTombstone(5)]).await;
        let tombstoned = t.store.create_document(Some(doc(2)), json!({})).await;
        assert!(matches!(tombstoned, Err(StoreError::AlreadyExists(id)) if id == doc(2)));
    }

    #[tokio::test]
    async fn update_and_delete_reject_foreign_read_only_and_missing_documents() {
        let t = temp_store().await;
        seed_synced(&t.store, doc(1), SCOPE_OWN, Some(OTHER), json!({}), 1).await;
        let foreign = t.store.update_document(doc(1), json!({"x": 1})).await;
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
            t.store.update_document(doc(2), json!({"x": 1})).await,
            Err(StoreError::NotWritable(_))
        ));
        assert!(matches!(
            t.store.delete_document(doc(2)).await,
            Err(StoreError::NotWritable(_))
        ));

        assert!(matches!(
            t.store.update_document(doc(3), json!({})).await,
            Err(StoreError::NotFound(id)) if id == doc(3)
        ));
        let created = t.store.create_document(None, json!({})).await.unwrap();
        t.store.delete_document(created).await.unwrap();
        assert!(matches!(
            t.store.delete_document(created).await,
            Err(StoreError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn new_edit_unparks_rows() {
        let t = temp_store().await;
        seed_synced(&t.store, doc(1), SCOPE_OWN, Some(ME), json!({"n": 0}), 1).await;
        t.store
            .update_document(doc(1), json!({"n": 1}))
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
            .update_document(doc(1), json!({"n": 2}))
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
            .update_document(doc(1), json!({"n": 1}))
            .await
            .unwrap();

        let rows = snapshot(&t.store, doc(1)).await.rows;
        let newest = rows.last().unwrap();
        assert!(newest.mutation_id > planted);
        assert_eq!(newest.kind, RowKind::Update);

        t.store.delete_document(doc(1)).await.unwrap();
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
            .update_document(doc(1), json!({"n": 1}))
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
        t.store.delete_document(doc(1)).await.unwrap();
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
    async fn rapid_edits_keep_marker_order() {
        let t = temp_store().await;
        let doc_id = t
            .store
            .create_document(None, json!({"n": 0}))
            .await
            .unwrap();
        for n in 1..=200 {
            t.store
                .update_document(doc_id, json!({"n": n}))
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
            .create_document(None, json!({"n": 0}))
            .await
            .unwrap();
        let mine = async {
            for n in 0..50 {
                t.store
                    .update_document(doc_id, json!({"a": n}))
                    .await
                    .unwrap();
            }
        };
        let theirs = async {
            for n in 0..50 {
                other
                    .update_document(doc_id, json!({"b": n}))
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
            store.create_document(None, json!({"n": n})).await.unwrap();
            store.close().await;
        }
        let reopened = open_again(&t.path()).await;
        assert_eq!(count(&reopened, "SELECT COUNT(*) FROM documents").await, 20);
    }

    /// A synced doc edited locally at `local`, then another device's `theirs` arrives.
    async fn collide(t: &TempStore, local: Value, theirs: Value) -> Vec<crate::store::DocNotice> {
        seed_synced(
            &t.store,
            doc(1),
            SCOPE_OWN,
            Some(ME),
            json!({"s": "old", "k": 1}),
            1,
        )
        .await;
        t.store.update_document(doc(1), local).await.unwrap();
        let change = upsert_change(SCOPE_OWN, envelope(doc(1), Some(ME), theirs, 2));
        t.store
            .apply_changes(ME, SCOPE_OWN, &[change], 2)
            .await
            .unwrap()
    }

    async fn kept_copy_id(t: &TempStore) -> i64 {
        sqlx::query_scalar("SELECT id FROM recovered WHERE reason = 'field_conflict'")
            .fetch_one(&t.store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn field_conflict_is_kept_with_its_paths() {
        let t = temp_store().await;
        let notices = collide(
            &t,
            json!({"s": "mine", "k": 2}),
            json!({"s": "theirs", "k": 1}),
        )
        .await;
        assert_eq!(
            notices,
            vec![crate::store::DocNotice {
                doc_id: doc(1),
                event: crate::engine::doc::DocEvent::FieldConflict {
                    paths: vec!["/s".into()]
                },
                kept: Some(crate::store::KeptCopy {
                    recovered_id: 1,
                    reason: "field_conflict".into()
                })
            }]
        );
        let fields: String = sqlx::query_scalar("SELECT fields FROM recovered WHERE doc_id = ?")
            .bind(doc(1).to_string())
            .fetch_one(&t.store.pool)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&fields).unwrap(),
            json!([{"path": "/s", "local_value": "mine", "local_removed": false}])
        );
        assert_eq!(
            snapshot(&t.store, doc(1)).await.content,
            json!({"s": "theirs", "k": 2})
        );
    }

    #[tokio::test]
    async fn restore_fields_writes_the_kept_values_back_as_a_local_edit() {
        let t = temp_store().await;
        collide(
            &t,
            json!({"s": "mine", "k": 2}),
            json!({"s": "theirs", "k": 1}),
        )
        .await;
        let rows_before = count(&t.store, "SELECT COUNT(*) FROM outbox").await;
        t.store
            .restore_fields(kept_copy_id(&t).await)
            .await
            .unwrap();
        assert_eq!(
            snapshot(&t.store, doc(1)).await.content,
            json!({"s": "mine", "k": 2})
        );
        assert_eq!(count(&t.store, "SELECT COUNT(*) FROM recovered").await, 0);
        assert_eq!(
            count(&t.store, "SELECT COUNT(*) FROM outbox").await,
            rows_before + 1
        );
    }

    #[tokio::test]
    async fn restore_fields_of_a_removed_value_removes_the_path() {
        let t = temp_store().await;
        collide(&t, json!({"k": 1}), json!({"s": "theirs", "k": 1})).await;
        t.store
            .restore_fields(kept_copy_id(&t).await)
            .await
            .unwrap();
        assert_eq!(snapshot(&t.store, doc(1)).await.content, json!({"k": 1}));
    }

    #[tokio::test]
    async fn restoring_a_whole_list_conflict_restores_the_list_exactly() {
        let t = temp_store().await;
        let scale = json!({"pitches": [0, 200, 400, 500, 700, 900]});
        seed_synced(&t.store, doc(1), SCOPE_OWN, Some(ME), scale, 1).await;
        let mine = json!({"pitches": [0, 200, 500, 700, 900]});
        t.store.update_document(doc(1), mine.clone()).await.unwrap();
        let theirs = json!({"pitches": [0, 200, 400, 500, 700]});
        let change = upsert_change(SCOPE_OWN, envelope(doc(1), Some(ME), theirs.clone(), 2));
        let notices = t
            .store
            .apply_changes(ME, SCOPE_OWN, &[change], 2)
            .await
            .unwrap();
        assert_eq!(
            notices,
            vec![crate::store::DocNotice {
                doc_id: doc(1),
                event: crate::engine::doc::DocEvent::FieldConflict {
                    paths: vec!["/pitches".into()]
                },
                kept: Some(crate::store::KeptCopy {
                    recovered_id: 1,
                    reason: "field_conflict".into()
                })
            }]
        );
        assert_eq!(snapshot(&t.store, doc(1)).await.content, theirs);
        t.store
            .restore_fields(kept_copy_id(&t).await)
            .await
            .unwrap();
        assert_eq!(
            snapshot(&t.store, doc(1)).await.content,
            mine,
            "the whole local list, exactly"
        );
    }

    #[tokio::test]
    async fn restore_fields_refuses_a_whole_document_copy() {
        let t = temp_store().await;
        seed_synced(&t.store, doc(1), SCOPE_OWN, Some(ME), json!({}), 1).await;
        exec(
            &t.store,
            &format!(
                "INSERT INTO recovered (doc_id, content, reason, recovered_at) VALUES ('{}', '{{}}', 'conflict', 0)",
                doc(1)
            ),
        )
        .await;
        let id: i64 = sqlx::query_scalar("SELECT id FROM recovered")
            .fetch_one(&t.store.pool)
            .await
            .unwrap();
        assert!(matches!(
            t.store.restore_fields(id).await,
            Err(StoreError::NoFieldConflict(refused)) if refused == id
        ));
        assert!(matches!(
            t.store.restore_fields(id + 1).await,
            Err(StoreError::NoKeptCopy(missing)) if missing == id + 1
        ));
    }

    async fn keep_whole(store: &Store, content: &str) -> i64 {
        exec(
            store,
            &format!(
                "INSERT INTO recovered (doc_id, content, reason, recovered_at) \
                 VALUES ('{}', '{content}', 'delete_wins', 1)",
                doc(9)
            ),
        )
        .await;
        store.list_recovered().await.unwrap()[0].id
    }

    #[tokio::test]
    async fn restore_document_creates_a_new_document_and_removes_the_copy() {
        let t = temp_store().await;
        let recovered_id = keep_whole(&t.store, r#"{"title":"Mine"}"#).await;
        let restored = t.store.restore_document(recovered_id).await.unwrap();
        assert_ne!(
            restored,
            doc(9),
            "a new id: the old one may be tombstoned on the server"
        );
        let snap = snapshot(&t.store, restored).await;
        assert_eq!(snap.content, json!({"title": "Mine"}));
        assert_eq!(snap.owner_id, Some(ME));
        assert_eq!(
            snap.rows.iter().map(|row| row.kind).collect::<Vec<_>>(),
            vec![RowKind::Create]
        );
        assert!(t.store.list_recovered().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn restore_document_takes_a_field_copy_whole_and_refuses_an_unknown_id() {
        let t = temp_store().await;
        exec(
            &t.store,
            &format!(
                "INSERT INTO recovered (doc_id, content, reason, recovered_at, fields) \
                 VALUES ('{}', '{{\"s\":\"mine\"}}', 'field_conflict', 1, '[]')",
                doc(1)
            ),
        )
        .await;
        assert!(matches!(
            t.store.restore_document(999).await,
            Err(StoreError::NoKeptCopy(999))
        ));
        let field_copy = t.store.list_recovered().await.unwrap()[0].id;
        let restored = t.store.restore_document(field_copy).await.unwrap();
        assert_eq!(
            snapshot(&t.store, restored).await.content,
            json!({"s": "mine"})
        );
        assert!(t.store.list_recovered().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn restoring_fields_of_a_deleted_document_reports_it_gone_and_keeps_the_copy() {
        let t = temp_store().await;
        exec(
            &t.store,
            &format!(
                "INSERT INTO recovered (doc_id, content, reason, recovered_at, fields) \
                 VALUES ('{}', '{{}}', 'field_conflict', 1, \
                 '[{{\"path\":\"/s\",\"local_value\":\"mine\",\"local_removed\":false}}]')",
                doc(1)
            ),
        )
        .await;
        let recovered_id = t.store.list_recovered().await.unwrap()[0].id;
        assert!(matches!(
            t.store.restore_fields(recovered_id).await,
            Err(StoreError::DocumentGone(id)) if id == doc(1)
        ));
        assert_eq!(t.store.list_recovered().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn restore_fields_records_a_local_change_and_unparks_the_rows() {
        let t = temp_store().await;
        let doc_id = t
            .store
            .create_document(None, json!({"s": "theirs", "k": 1}))
            .await
            .unwrap();
        exec(
            &t.store,
            &format!("UPDATE outbox SET parked_error = 'validation' WHERE doc_id = '{doc_id}'"),
        )
        .await;
        exec(
            &t.store,
            &format!(
                "INSERT INTO recovered (doc_id, content, reason, recovered_at, fields) \
                 VALUES ('{doc_id}', '{{}}', 'field_conflict', 1, \
                 '[{{\"path\":\"/s\",\"local_value\":\"mine\",\"local_removed\":false}}]')"
            ),
        )
        .await;
        let recovered_id = t.store.list_recovered().await.unwrap()[0].id;
        t.store.restore_fields(recovered_id).await.unwrap();
        assert_eq!(
            snapshot(&t.store, doc_id).await.content,
            json!({"s": "mine", "k": 1})
        );
        assert_eq!(
            count(
                &t.store,
                "SELECT COUNT(*) FROM outbox WHERE parked_error IS NOT NULL"
            )
            .await,
            0
        );
        let last: (String, String) =
            sqlx::query_as("SELECT doc_id, origin FROM change_log ORDER BY local_seq DESC LIMIT 1")
                .fetch_one(&t.store.pool)
                .await
                .unwrap();
        assert_eq!(last, (doc_id.to_string(), "local".to_string()));
    }
}
