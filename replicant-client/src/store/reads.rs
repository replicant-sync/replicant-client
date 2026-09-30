//! What the host reads: documents as the host sees them (a pending delete hides one), counts,
//! and full-text search over the configured JSON paths.

use serde::Serialize;
use serde_json::Value;
use sqlx::sqlite::SqliteRow;
use sqlx::Row;
use uuid::Uuid;

use super::uploads::PENDING_DOC_IDS;
use super::{Store, StoreResult};
use crate::engine::types::SCOPE_CURATED;

pub(crate) const HAS_SEARCH_CONFIG: &str = "SELECT EXISTS(SELECT 1 FROM search_config LIMIT 1)";
pub(crate) const DELETE_FTS_ENTRY: &str = "DELETE FROM documents_fts WHERE document_id = ?";
pub(crate) const UPDATE_FTS_ENTRY: &str = "INSERT INTO documents_fts (document_id, title, body) \
     SELECT d.id, COALESCE(d.title, ''), COALESCE((SELECT GROUP_CONCAT(json_extract(d.content, \
     sc.json_path), ' ') FROM search_config sc WHERE json_extract(d.content, sc.json_path) IS NOT NULL), '') \
     FROM documents d WHERE d.id = ? AND d.deleted_at IS NULL";
const REBUILD_FTS_INDEX: &str = "INSERT INTO documents_fts (document_id, title, body) \
     SELECT d.id, COALESCE(d.title, ''), COALESCE((SELECT GROUP_CONCAT(json_extract(d.content, \
     sc.json_path), ' ') FROM search_config sc WHERE json_extract(d.content, sc.json_path) IS NOT NULL), '') \
     FROM documents d WHERE d.deleted_at IS NULL";

/// A document as the host sees it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoredDocument {
    pub id: Uuid,
    /// The owner; `None` for a legacy document that has none.
    pub user_id: Option<Uuid>,
    pub author_id: Option<Uuid>,
    pub title: Option<String>,
    pub content: Value,
    pub read_only: bool,
    /// `public` for a curated or read-only document, `private` otherwise.
    pub visibility: &'static str,
    pub source_doc_id: Option<Uuid>,
    pub derived_from: Option<Uuid>,
    pub created_at: String,
    pub updated_at: String,
}

/// A document that stopped uploading until its next local edit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParkedDocument {
    pub doc_id: Uuid,
    pub code: String,
}

/// Visible documents (no pending delete), after `join`, narrowed by `filter` (`AND …`, may be empty).
fn select_documents(join: &str, filter: &str) -> String {
    format!(
        "SELECT d.id, d.user_id, d.author_id, d.title, d.content, d.read_only, d.source_doc_id, \
         d.derived_from, d.created_at, d.updated_at, (d.read_only = 1 OR EXISTS (SELECT 1 FROM \
         doc_scopes s WHERE s.doc_id = d.id AND s.scope = '{SCOPE_CURATED}' AND s.member = 1)) \
         AS public FROM documents d {join} WHERE d.deleted_at IS NULL {filter}"
    )
}

fn parse_document(row: &SqliteRow) -> StoreResult<StoredDocument> {
    let uuid = |column: &str| -> StoreResult<Option<Uuid>> {
        Ok(row
            .try_get::<Option<String>, _>(column)?
            .map(|text| Uuid::parse_str(&text))
            .transpose()?)
    };
    Ok(StoredDocument {
        id: Uuid::parse_str(&row.try_get::<String, _>("id")?)?,
        user_id: uuid("user_id")?,
        author_id: uuid("author_id")?,
        title: row.try_get("title")?,
        content: serde_json::from_str(&row.try_get::<String, _>("content")?)?,
        read_only: row.try_get::<i64, _>("read_only")? != 0,
        visibility: if row.try_get::<i64, _>("public")? != 0 {
            "public"
        } else {
            "private"
        },
        source_doc_id: uuid("source_doc_id")?,
        derived_from: uuid("derived_from")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

impl Store {
    pub async fn get_document(&self, doc_id: Uuid) -> StoreResult<Option<StoredDocument>> {
        let row = sqlx::query(&select_documents("", "AND d.id = ?"))
            .bind(doc_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(parse_document).transpose()
    }

    pub async fn list_documents(&self) -> StoreResult<Vec<StoredDocument>> {
        let rows = sqlx::query(&select_documents("", "ORDER BY d.id"))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(parse_document).collect()
    }

    /// `include_deleted` adds documents whose delete is not yet sent.
    pub async fn document_ids(&self, include_deleted: bool) -> StoreResult<Vec<Uuid>> {
        let sql = if include_deleted {
            "SELECT id FROM documents ORDER BY id"
        } else {
            "SELECT id FROM documents WHERE deleted_at IS NULL ORDER BY id"
        };
        let ids: Vec<String> = sqlx::query_scalar(sql).fetch_all(&self.pool).await?;
        ids.iter().map(|id| Ok(Uuid::parse_str(id)?)).collect()
    }

    pub async fn count_documents(&self) -> StoreResult<u64> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM documents WHERE deleted_at IS NULL")
                .fetch_one(&self.pool)
                .await?;
        Ok(count as u64)
    }

    /// Documents the uploader will send. A parked document waits for the user's next edit
    /// (`list_parked`).
    pub async fn count_pending_sync(&self) -> StoreResult<u64> {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM ({PENDING_DOC_IDS})"))
            .fetch_one(&self.pool)
            .await?;
        Ok(count as u64)
    }

    /// Documents that stopped uploading until their next local edit, with the code of their
    /// newest parked row (`validation`, `forbidden`, `too_large`, `diverged`).
    pub async fn list_parked(&self) -> StoreResult<Vec<ParkedDocument>> {
        let rows: Vec<(String, String)> = sqlx::query_as(&format!(
            "SELECT p.doc_id, (SELECT n.parked_error FROM outbox n WHERE n.doc_id = p.doc_id \
                 AND n.parked_error IS NOT NULL ORDER BY n.mutation_id DESC LIMIT 1) \
             FROM (SELECT DISTINCT doc_id FROM outbox WHERE parked_error IS NOT NULL) p \
             WHERE p.doc_id NOT IN ({PENDING_DOC_IDS}) ORDER BY p.doc_id"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(doc_id, code)| {
                Ok(ParkedDocument {
                    doc_id: Uuid::parse_str(&doc_id)?,
                    code,
                })
            })
            .collect()
    }

    /// Replaces the indexed JSON paths and rebuilds the index, in one transaction.
    pub async fn configure_search(&self, json_paths: &[String]) -> StoreResult<()> {
        let mut tx = self.begin().await?;
        sqlx::query("DELETE FROM search_config")
            .execute(&mut *tx)
            .await?;
        for path in json_paths {
            sqlx::query("INSERT INTO search_config (json_path) VALUES (?)")
                .bind(path)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM documents_fts")
            .execute(&mut *tx)
            .await?;
        sqlx::query(REBUILD_FTS_INDEX).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn rebuild_search_index(&self) -> StoreResult<()> {
        let mut tx = self.begin().await?;
        sqlx::query("DELETE FROM documents_fts")
            .execute(&mut *tx)
            .await?;
        sqlx::query(REBUILD_FTS_INDEX).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// FTS5 query syntax: terms, `prefix*`, `"a phrase"`, `AND`/`OR`, `title:word`.
    pub async fn search_documents(
        &self,
        query: &str,
        limit: u32,
    ) -> StoreResult<Vec<StoredDocument>> {
        let rows = sqlx::query(&select_documents(
            "JOIN documents_fts f ON f.document_id = d.id",
            "AND f.documents_fts MATCH ? ORDER BY f.rank LIMIT ?",
        ))
        .bind(query)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(parse_document).collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use crate::engine::machine::BuildOutcome;
    use crate::engine::types::{ServerError, SCOPE_CURATED, SCOPE_OWN};
    use crate::store::test_support::*;
    use crate::store::{ParkedDocument, Store};

    fn doc(n: u128) -> Uuid {
        Uuid::from_u128(0xD000 + n)
    }

    #[tokio::test]
    async fn get_document_returns_the_host_view_and_hides_a_pending_delete() {
        let t = temp_store().await;
        let doc_id = t
            .store
            .create_document(None, json!({"title": "Mine", "n": 1}))
            .await
            .unwrap();
        let read = t
            .store
            .get_document(doc_id)
            .await
            .unwrap()
            .expect("created");
        assert_eq!(read.id, doc_id);
        assert_eq!(read.user_id, Some(ME));
        assert_eq!(read.title.as_deref(), Some("Mine"));
        assert_eq!(read.content, json!({"title": "Mine", "n": 1}));
        assert!(!read.read_only);
        assert_eq!(read.visibility, "private");
        assert_eq!(t.store.list_documents().await.unwrap(), vec![read]);
        t.store.delete_document(doc_id).await.unwrap();
        assert_eq!(t.store.get_document(doc_id).await.unwrap(), None);
        assert!(t.store.list_documents().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn curated_and_read_only_documents_are_public() {
        let t = temp_store().await;
        seed_synced(&t.store, doc(1), SCOPE_CURATED, None, json!({"n": 1}), 3).await;
        seed_synced(&t.store, doc(2), SCOPE_OWN, Some(ME), json!({"n": 2}), 4).await;
        exec(
            &t.store,
            &format!("UPDATE documents SET read_only = 1 WHERE id = '{}'", doc(2)),
        )
        .await;
        seed_synced(&t.store, doc(3), SCOPE_OWN, Some(ME), json!({"n": 3}), 5).await;
        for (n, expected) in [(1, "public"), (2, "public"), (3, "private")] {
            let read = t.store.get_document(doc(n)).await.unwrap().unwrap();
            assert_eq!(read.visibility, expected, "doc {n}");
        }
    }

    #[tokio::test]
    async fn document_ids_can_include_pending_deletes() {
        let t = temp_store().await;
        let kept = t.store.create_document(None, json!({})).await.unwrap();
        let deleted = t.store.create_document(None, json!({})).await.unwrap();
        t.store.delete_document(deleted).await.unwrap();
        assert_eq!(t.store.document_ids(false).await.unwrap(), vec![kept]);
        let mut all = t.store.document_ids(true).await.unwrap();
        all.sort();
        let mut expected = vec![kept, deleted];
        expected.sort();
        assert_eq!(all, expected);
    }

    #[tokio::test]
    async fn counts_hide_pending_deletes_and_pending_sync_skips_parked_documents() {
        let t = temp_store().await;
        seed_synced(&t.store, doc(8), SCOPE_OWN, Some(ME), json!({}), 1).await;
        let first = doc(8);
        let parked = t.store.create_document(None, json!({})).await.unwrap();
        seed_synced(&t.store, doc(9), SCOPE_OWN, Some(ME), json!({}), 1).await;
        exec(
            &t.store,
            &format!("UPDATE outbox SET parked_error = 'validation' WHERE doc_id = '{parked}'"),
        )
        .await;
        assert_eq!(t.store.count_documents().await.unwrap(), 3);
        assert_eq!(
            t.store.count_pending_sync().await.unwrap(),
            0,
            "a parked document waits for an edit, not for sync; a synced one does not count"
        );
        t.store.delete_document(first).await.unwrap();
        assert_eq!(t.store.count_documents().await.unwrap(), 2);
        assert_eq!(
            t.store.count_pending_sync().await.unwrap(),
            1,
            "its delete is pending"
        );
    }

    #[tokio::test]
    async fn list_parked_names_each_parked_document_and_its_code() {
        let t = temp_store().await;
        let parked = t.store.create_document(None, json!({})).await.unwrap();
        t.store.create_document(None, json!({})).await.unwrap();
        exec(
            &t.store,
            &format!("UPDATE outbox SET parked_error = 'too_large' WHERE doc_id = '{parked}'"),
        )
        .await;
        assert_eq!(
            t.store.list_parked().await.unwrap(),
            vec![ParkedDocument {
                doc_id: parked,
                code: "too_large".into()
            }]
        );
    }

    /// Sends the document's queued rows and has the server refuse them as invalid.
    async fn refuse_upload(t: &TempStore, doc_id: Uuid) {
        let BuildOutcome::Send { inflight, .. } = t.store.build_upload(ME, doc_id).await.unwrap()
        else {
            panic!("expected an upload");
        };
        t.store
            .settle_upload(
                ME,
                doc_id,
                &inflight,
                &Err(ServerError::new("validation")),
                0,
                0,
            )
            .await
            .unwrap();
    }

    async fn parked_ids(store: &Store) -> Vec<Uuid> {
        let parked = store.list_parked().await.unwrap();
        parked.into_iter().map(|parked| parked.doc_id).collect()
    }

    #[tokio::test]
    async fn an_edit_made_during_a_refused_upload_leaves_the_document_parked_not_pending() {
        let t = temp_store().await;
        seed_synced(&t.store, doc(1), SCOPE_OWN, Some(ME), json!({"n": 0}), 1).await;
        t.store
            .update_document(doc(1), json!({"n": 1}))
            .await
            .unwrap();
        let BuildOutcome::Send { inflight, .. } = t.store.build_upload(ME, doc(1)).await.unwrap()
        else {
            panic!("expected an upload");
        };
        t.store
            .update_document(doc(1), json!({"n": 2}))
            .await
            .unwrap();
        t.store
            .settle_upload(
                ME,
                doc(1),
                &inflight,
                &Err(ServerError::new("validation")),
                0,
                0,
            )
            .await
            .unwrap();
        assert!(t.store.load_pending().await.unwrap().is_empty());
        assert_eq!(t.store.count_pending_sync().await.unwrap(), 0);
        assert_eq!(parked_ids(&t.store).await, vec![doc(1)]);
    }

    #[tokio::test]
    async fn a_delete_after_a_refused_upload_is_pending_and_the_document_stays_hidden() {
        let t = temp_store().await;
        seed_synced(&t.store, doc(1), SCOPE_OWN, Some(ME), json!({"n": 0}), 1).await;
        t.store
            .update_document(doc(1), json!({"n": 1}))
            .await
            .unwrap();
        let BuildOutcome::Send { inflight, .. } = t.store.build_upload(ME, doc(1)).await.unwrap()
        else {
            panic!("expected an upload");
        };
        t.store.delete_document(doc(1)).await.unwrap();
        t.store
            .settle_upload(
                ME,
                doc(1),
                &inflight,
                &Err(ServerError::new("validation")),
                0,
                0,
            )
            .await
            .unwrap();
        assert_eq!(t.store.load_pending().await.unwrap(), vec![doc(1)]);
        assert_eq!(t.store.count_pending_sync().await.unwrap(), 1);
        assert!(parked_ids(&t.store).await.is_empty());
        assert_eq!(t.store.get_document(doc(1)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn every_document_with_outbox_rows_is_exactly_one_of_pending_or_parked() {
        let t = temp_store().await;
        for n in 1..=5 {
            seed_synced(&t.store, doc(n), SCOPE_OWN, Some(ME), json!({"n": 0}), 1).await;
            t.store
                .update_document(doc(n), json!({"n": 1}))
                .await
                .unwrap();
        }
        // 1: queued. 2: refused. 3: edited during a refused upload. 4: refused then deleted.
        refuse_upload(&t, doc(2)).await;
        let BuildOutcome::Send { inflight, .. } = t.store.build_upload(ME, doc(3)).await.unwrap()
        else {
            panic!("expected an upload");
        };
        t.store
            .update_document(doc(3), json!({"n": 2}))
            .await
            .unwrap();
        t.store
            .settle_upload(
                ME,
                doc(3),
                &inflight,
                &Err(ServerError::new("forbidden")),
                0,
                0,
            )
            .await
            .unwrap();
        refuse_upload(&t, doc(4)).await;
        t.store.delete_document(doc(4)).await.unwrap();
        // 5: refused, deleted, and that delete parked too.
        refuse_upload(&t, doc(5)).await;
        t.store.delete_document(doc(5)).await.unwrap();
        exec(
            &t.store,
            &format!(
                "UPDATE outbox SET parked_error = 'forbidden' WHERE doc_id = '{}'",
                doc(5)
            ),
        )
        .await;

        let pending = t.store.load_pending().await.unwrap();
        let parked = parked_ids(&t.store).await;
        assert!(pending.iter().all(|id| !parked.contains(id)));
        let mut union: Vec<Uuid> = pending.iter().chain(&parked).copied().collect();
        union.sort();
        let with_rows: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT doc_id FROM outbox ORDER BY doc_id")
                .fetch_all(&t.store.pool)
                .await
                .unwrap();
        assert_eq!(
            union.iter().map(Uuid::to_string).collect::<Vec<_>>(),
            with_rows
        );
        assert_eq!(
            t.store.count_pending_sync().await.unwrap(),
            pending.len() as u64
        );
        assert_eq!(pending, vec![doc(1), doc(4)]);
    }

    #[tokio::test]
    async fn list_parked_reports_the_code_of_the_newest_parked_row() {
        let t = temp_store().await;
        let doc_id = t.store.create_document(None, json!({})).await.unwrap();
        exec(
            &t.store,
            &format!(
                "UPDATE outbox SET parked_error = 'too_large' WHERE doc_id = '{doc_id}'; \
                 INSERT INTO outbox (mutation_id, doc_id, kind, created_at, parked_error) \
                 VALUES ('00000000-0000-0000-0000-000000000000', '{doc_id}', 'update', 0, 'validation')"
            ),
        )
        .await;
        let code = t.store.list_parked().await.unwrap()[0].code.clone();
        let newest: String =
            sqlx::query_scalar("SELECT parked_error FROM outbox ORDER BY mutation_id DESC LIMIT 1")
                .fetch_one(&t.store.pool)
                .await
                .unwrap();
        assert_eq!(code, newest);
    }

    #[tokio::test]
    async fn search_finds_configured_paths_and_skips_pending_deletes() {
        let t = temp_store().await;
        t.store
            .configure_search(&["$.body".to_string()])
            .await
            .unwrap();
        let music = t
            .store
            .create_document(None, json!({"title": "Alpha", "body": "music theory"}))
            .await
            .unwrap();
        t.store
            .create_document(None, json!({"title": "Beta", "body": "cooking"}))
            .await
            .unwrap();
        let ids = |found: Vec<crate::store::StoredDocument>| {
            found
                .into_iter()
                .map(|document| document.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(t.store.search_documents("music", 10).await.unwrap()),
            vec![music]
        );
        t.store.rebuild_search_index().await.unwrap();
        assert_eq!(
            ids(t.store.search_documents("music", 10).await.unwrap()),
            vec![music]
        );
        t.store.delete_document(music).await.unwrap();
        assert!(t
            .store
            .search_documents("music", 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn search_returns_the_best_match_first_when_limited() {
        let t = temp_store().await;
        t.store
            .configure_search(&["$.body".to_string()])
            .await
            .unwrap();
        let weak = Uuid::from_u128(0x1);
        let strong = Uuid::from_u128(0xFFFF);
        t.store
            .create_document(
                Some(weak),
                json!({"body": "one mention of tuning among many other unrelated words here"}),
            )
            .await
            .unwrap();
        t.store
            .create_document(Some(strong), json!({"body": "tuning tuning tuning"}))
            .await
            .unwrap();
        let found = t.store.search_documents("tuning", 1).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, strong);
    }
}
