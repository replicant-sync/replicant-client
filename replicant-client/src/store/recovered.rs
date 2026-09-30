//! Local content the sync rules set aside (`DocOp::Recover`), kept until the user dismisses it.

use serde_json::Value;
use sqlx::sqlite::SqliteRow;
use sqlx::Row;
use tracing::warn;
use uuid::Uuid;

use super::{Store, StoreResult};
use crate::engine::doc::FieldConflict;

#[derive(Debug, Clone, PartialEq)]
pub struct RecoveredCopy {
    pub id: i64,
    pub doc_id: Uuid,
    /// The full local content when it was set aside.
    pub content: Value,
    /// `conflict`, `field_conflict`, `delete_wins`, `became_publication`, `create_rejected`,
    /// `delete_superseded`, `delete_refused`, `delete_publication` or `unmigratable`.
    pub reason: String,
    /// Unix seconds.
    pub recovered_at: i64,
    /// For `field_conflict`: the colliding paths and local values (`Store::restore_fields`).
    pub fields: Option<Vec<FieldConflict>>,
}

fn parse_copy(row: &SqliteRow) -> StoreResult<RecoveredCopy> {
    Ok(RecoveredCopy {
        id: row.try_get("id")?,
        doc_id: Uuid::parse_str(&row.try_get::<String, _>("doc_id")?)?,
        content: serde_json::from_str(&row.try_get::<String, _>("content")?)?,
        reason: row.try_get("reason")?,
        recovered_at: row.try_get("recovered_at")?,
        fields: row
            .try_get::<Option<String>, _>("fields")?
            .map(|fields| serde_json::from_str(&fields))
            .transpose()?,
    })
}

impl Store {
    /// Every kept copy, newest first. A row that cannot be read is left out (and logged), so one
    /// bad row never hides the others.
    pub async fn list_recovered(&self) -> StoreResult<Vec<RecoveredCopy>> {
        let rows = sqlx::query(
            "SELECT id, doc_id, content, reason, recovered_at, fields FROM recovered ORDER BY id DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .filter_map(|row| match parse_copy(row) {
                Ok(copy) => Some(copy),
                Err(error) => {
                    let id = row.try_get::<i64, _>("id").ok();
                    warn!(?id, %error, "skipping an unreadable kept copy");
                    None
                }
            })
            .collect())
    }

    /// Deletes one kept copy; false when no copy has that id.
    pub async fn dismiss_recovered(&self, id: i64) -> StoreResult<bool> {
        let result = sqlx::query("DELETE FROM recovered WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use crate::store::test_support::*;
    use crate::store::Store;

    async fn keep(store: &Store, doc_id: Uuid, content: &str, recovered_at: i64) {
        exec(
            store,
            &format!(
                "INSERT INTO recovered (doc_id, content, reason, recovered_at) \
                 VALUES ('{doc_id}', '{content}', 'conflict', {recovered_at})"
            ),
        )
        .await;
    }

    #[tokio::test]
    async fn list_recovered_returns_every_copy_newest_first() {
        let t = temp_store().await;
        keep(&t.store, Uuid::from_u128(1), r#"{"v":1}"#, 100).await;
        keep(&t.store, Uuid::from_u128(2), r#"{"v":2}"#, 200).await;
        let copies = t.store.list_recovered().await.unwrap();
        let doc_ids: Vec<Uuid> = copies.iter().map(|copy| copy.doc_id).collect();
        assert_eq!(doc_ids, vec![Uuid::from_u128(2), Uuid::from_u128(1)]);
        assert_eq!(copies[0].content, json!({"v": 2}));
        assert_eq!(copies[0].reason, "conflict");
        assert_eq!(copies[0].recovered_at, 200);
        assert_eq!(
            copies[0].fields, None,
            "a whole-document copy names no fields"
        );
    }

    #[tokio::test]
    async fn an_unreadable_kept_copy_is_left_out_and_the_others_are_listed() {
        let t = temp_store().await;
        keep(&t.store, Uuid::from_u128(1), r#"{"v":1}"#, 100).await;
        keep(&t.store, Uuid::from_u128(2), "not json", 200).await;
        exec(
            &t.store,
            "INSERT INTO recovered (doc_id, content, reason, recovered_at) \
             VALUES ('not-a-uuid', '{}', 'conflict', 300)",
        )
        .await;
        keep(&t.store, Uuid::from_u128(4), r#"{"v":4}"#, 400).await;
        let doc_ids: Vec<Uuid> = t
            .store
            .list_recovered()
            .await
            .unwrap()
            .iter()
            .map(|copy| copy.doc_id)
            .collect();
        assert_eq!(doc_ids, vec![Uuid::from_u128(4), Uuid::from_u128(1)]);
    }

    #[tokio::test]
    async fn dismiss_recovered_deletes_exactly_one_row() {
        let t = temp_store().await;
        keep(&t.store, Uuid::from_u128(1), "{}", 100).await;
        keep(&t.store, Uuid::from_u128(2), "{}", 200).await;
        let dismissed = t.store.list_recovered().await.unwrap()[0].id;
        assert!(t.store.dismiss_recovered(dismissed).await.unwrap());
        assert!(
            !t.store.dismiss_recovered(dismissed).await.unwrap(),
            "already gone"
        );
        let left = t.store.list_recovered().await.unwrap();
        assert_eq!(left.len(), 1);
        assert_ne!(left[0].id, dismissed);
    }
}
