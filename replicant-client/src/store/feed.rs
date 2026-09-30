//! Download effects: cursors, change pages and pushes, snapshot pages and the resync sweep.

use std::collections::HashSet;

use sqlx::SqliteConnection;
use uuid::Uuid;

use super::docs::{apply_ops, load_snapshot, LogOrigin};
use super::{DocNotice, Store, StoreResult};
use crate::engine::doc::{apply_change, apply_snapshot_doc, sweep_doc};
use crate::engine::types::{Change, DocEnvelope, Scope, Seq};

impl Store {
    /// The scopes to build the core with, in subscription order.
    pub async fn subscribed_scopes(&self) -> StoreResult<Vec<Scope>> {
        Ok(
            sqlx::query_scalar("SELECT scope FROM subscriptions ORDER BY rowid")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// `Effect::LoadCursors`, answered with `Input::Cursors`.
    pub async fn load_cursors(&self) -> StoreResult<Vec<(Scope, Seq)>> {
        Ok(
            sqlx::query_as("SELECT scope, cursor FROM subscriptions ORDER BY rowid")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// `Effect::ApplyChanges`: the changes and the cursor commit together.
    pub async fn apply_changes(
        &self,
        me: Uuid,
        scope: &str,
        changes: &[Change],
        new_cursor: Seq,
    ) -> StoreResult<Vec<DocNotice>> {
        let writer = self.writer(LogOrigin::Server);
        let mut tx = self.begin().await?;
        let mut notices = Vec::new();
        for change in changes {
            let snap = load_snapshot(&mut tx, change.doc_id).await?;
            let ops = apply_change(&snap, change, me, &self.list_merge);
            notices.extend(apply_ops(&mut tx, &writer, &snap, &ops, change.doc.as_ref()).await?);
        }
        advance_cursor(&mut tx, scope, new_cursor).await?;
        tx.commit().await?;
        Ok(notices)
    }

    /// `Effect::ApplySnapshotPage`: each document through `apply_snapshot_doc` at its own seq
    /// (both guards; pending local edits go to `recovered`). The cursor moves at `finish_snapshot`.
    pub async fn apply_snapshot_page(
        &self,
        me: Uuid,
        scope: &str,
        docs: &[DocEnvelope],
    ) -> StoreResult<Vec<DocNotice>> {
        let writer = self.writer(LogOrigin::Server);
        let mut tx = self.begin().await?;
        let mut notices = Vec::new();
        for doc in docs {
            let snap = load_snapshot(&mut tx, doc.doc_id).await?;
            let ops = apply_snapshot_doc(&snap, scope, doc, me, &self.list_merge);
            notices.extend(apply_ops(&mut tx, &writer, &snap, &ops, Some(doc)).await?);
        }
        tx.commit().await?;
        Ok(notices)
    }

    /// `Effect::FinishSnapshot`: sweep members the snapshot did not list, then persist
    /// `snapshot_seq` so an empty scope is not snapshotted again next connection.
    pub async fn finish_snapshot(
        &self,
        scope: &str,
        seen: &[Uuid],
        snapshot_seq: Seq,
    ) -> StoreResult<()> {
        let writer = self.writer(LogOrigin::Server);
        let seen: HashSet<Uuid> = seen.iter().copied().collect();
        let mut tx = self.begin().await?;
        let members: Vec<String> = sqlx::query_scalar(
            "SELECT doc_id FROM doc_scopes WHERE scope = ? AND member = 1 AND seq <= ?",
        )
        .bind(scope)
        .bind(snapshot_seq)
        .fetch_all(&mut *tx)
        .await?;
        for member in members {
            let doc_id = Uuid::parse_str(&member)?;
            if seen.contains(&doc_id) {
                continue;
            }
            let snap = load_snapshot(&mut tx, doc_id).await?;
            let ops = sweep_doc(&snap, scope, snapshot_seq);
            apply_ops(&mut tx, &writer, &snap, &ops, None).await?;
        }
        advance_cursor(&mut tx, scope, snapshot_seq).await?;
        tx.commit().await?;
        Ok(())
    }

    /// `Effect::DropSubscription`; no answer.
    pub async fn drop_subscription(&self, scope: &str) -> StoreResult<()> {
        sqlx::query("DELETE FROM subscriptions WHERE scope = ?")
            .bind(scope)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// Cursors are shared by every process on the data dir and never move backwards.
async fn advance_cursor(conn: &mut SqliteConnection, scope: &str, cursor: Seq) -> StoreResult<()> {
    sqlx::query("UPDATE subscriptions SET cursor = max(cursor, ?) WHERE scope = ?")
        .bind(cursor)
        .bind(scope)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use crate::engine::doc::{DocOp, Membership};
    use crate::engine::types::{SCOPE_CURATED, SCOPE_OWN};
    use crate::store::test_support::*;

    fn doc(n: u128) -> Uuid {
        Uuid::from_u128(0xD000 + n)
    }

    async fn cursor(t: &TempStore, scope: &str) -> i64 {
        let cursors = t.store.load_cursors().await.unwrap();
        cursors
            .into_iter()
            .find(|(name, _)| name == scope)
            .map(|(_, cursor)| cursor)
            .expect("subscribed scope")
    }

    #[tokio::test]
    async fn apply_changes_commits_documents_and_cursor_together() {
        let t = temp_store().await;
        let changes = vec![
            upsert_change(
                SCOPE_OWN,
                envelope(doc(1), Some(ME), json!({"title": "One"}), 11),
            ),
            upsert_change(
                SCOPE_OWN,
                envelope(doc(2), Some(ME), json!({"title": "Two"}), 12),
            ),
        ];
        t.store
            .apply_changes(ME, SCOPE_OWN, &changes, 12)
            .await
            .unwrap();
        assert_eq!(
            snapshot(&t.store, doc(2)).await.content,
            json!({"title": "Two"})
        );
        assert_eq!(cursor(&t, SCOPE_OWN).await, 12);
        assert_eq!(cursor(&t, SCOPE_CURATED).await, 0);
    }

    #[tokio::test]
    async fn cursor_only_moves_forward() {
        let t = temp_store().await;
        t.store.apply_changes(ME, SCOPE_OWN, &[], 20).await.unwrap();
        t.store.apply_changes(ME, SCOPE_OWN, &[], 15).await.unwrap();
        assert_eq!(cursor(&t, SCOPE_OWN).await, 20);
    }

    #[tokio::test]
    async fn two_processes_applying_one_page_converge() {
        let t = temp_store().await;
        let other = open_again(&t.path()).await;
        let page = vec![upsert_change(
            SCOPE_OWN,
            envelope(doc(1), Some(ME), json!({"v": 2}), 7),
        )];
        t.store
            .apply_changes(ME, SCOPE_OWN, &page, 7)
            .await
            .unwrap();
        other.apply_changes(ME, SCOPE_OWN, &page, 7).await.unwrap();
        let loaded = snapshot(&t.store, doc(1)).await;
        assert_eq!(loaded.content, json!({"v": 2}));
        assert_eq!(loaded.shadow.unwrap().seq, 7);
        assert_eq!(
            count(&t.store, "SELECT COUNT(*) FROM change_log").await,
            1,
            "the second apply is skipped by the content guard"
        );
    }

    #[tokio::test]
    async fn snapshot_page_applies_with_both_guards() {
        let t = temp_store().await;
        seed_synced(
            &t.store,
            doc(1),
            SCOPE_CURATED,
            None,
            json!({"v": "newer"}),
            9,
        )
        .await;
        let docs = vec![
            envelope(doc(1), None, json!({"v": "older"}), 5),
            envelope(doc(2), None, json!({"v": "new"}), 6),
        ];
        t.store
            .apply_snapshot_page(ME, SCOPE_CURATED, &docs)
            .await
            .unwrap();
        assert_eq!(
            snapshot(&t.store, doc(1)).await.content,
            json!({"v": "newer"})
        );
        let fresh = snapshot(&t.store, doc(2)).await;
        assert_eq!(fresh.content, json!({"v": "new"}));
        assert_eq!(
            fresh.memberships,
            vec![Membership {
                scope: SCOPE_CURATED.into(),
                member: true,
                seq: 6
            }]
        );
        assert_eq!(cursor(&t, SCOPE_CURATED).await, 0);
    }

    #[tokio::test]
    async fn snapshot_page_sends_pending_local_edits_to_recovered() {
        let t = temp_store().await;
        seed_synced(
            &t.store,
            doc(1),
            SCOPE_OWN,
            Some(ME),
            json!({"items": ["a"]}),
            1,
        )
        .await;
        let local = json!({"items": ["a", "b"]});
        t.store
            .update_document(doc(1), local.clone())
            .await
            .unwrap();
        // The append went out and its reply was lost.
        t.store.mark_sent(doc(1), Uuid::max()).await.unwrap();
        // The snapshot already holds the append (its upload landed, the reply was lost).
        let snapshot_doc = envelope(doc(1), Some(ME), json!({"items": ["a", "b"], "t": 1}), 9);

        let notices = t
            .store
            .apply_snapshot_page(ME, SCOPE_OWN, std::slice::from_ref(&snapshot_doc))
            .await
            .unwrap();

        assert_eq!(
            notices,
            vec![crate::store::DocNotice {
                doc_id: doc(1),
                event: crate::engine::doc::DocEvent::ConflictDetected
            }]
        );
        let recovered: Vec<(String, String)> =
            sqlx::query_as("SELECT content, reason FROM recovered WHERE doc_id = ?")
                .bind(doc(1).to_string())
                .fetch_all(&t.store.pool)
                .await
                .unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&recovered[0].0).unwrap(),
            local
        );
        assert_eq!(recovered[0].1, "conflict");
        let after = snapshot(&t.store, doc(1)).await;
        assert_eq!(after.content, snapshot_doc.content);
        assert!(after.rows.is_empty(), "no marker, nothing re-uploaded");
    }

    #[tokio::test]
    async fn returning_user_with_never_sent_edits_rebases_onto_the_snapshot() {
        let t = temp_store().await;
        seed_synced(
            &t.store,
            doc(1),
            SCOPE_OWN,
            Some(ME),
            json!({"a": 1, "b": 1}),
            1,
        )
        .await;
        t.store
            .update_document(doc(1), json!({"a": 2, "b": 1}))
            .await
            .unwrap();
        // Months later: another device changed `b`; the resync delivers it as a snapshot.
        let snapshot_doc = envelope(doc(1), Some(ME), json!({"a": 1, "b": 2}), 9);
        let notices = t
            .store
            .apply_snapshot_page(ME, SCOPE_OWN, std::slice::from_ref(&snapshot_doc))
            .await
            .unwrap();
        assert!(notices.is_empty(), "no ConflictDetected: {notices:?}");
        assert_eq!(count(&t.store, "SELECT COUNT(*) FROM recovered").await, 0);
        let after = snapshot(&t.store, doc(1)).await;
        assert_eq!(after.content, json!({"a": 2, "b": 2}));
        assert_eq!(after.rows.len(), 1, "the offline edit still uploads");
        assert_eq!(after.shadow.unwrap().seq, 9);
    }

    #[tokio::test]
    async fn sweep_removes_missing_docs_but_keeps_docs_with_outbox_rows() {
        let t = temp_store().await;
        seed_synced(
            &t.store,
            doc(1),
            SCOPE_CURATED,
            None,
            json!({"missing": true}),
            3,
        )
        .await;
        seed_synced(
            &t.store,
            doc(2),
            SCOPE_CURATED,
            None,
            json!({"seen": true}),
            3,
        )
        .await;
        seed_synced(
            &t.store,
            doc(3),
            SCOPE_CURATED,
            Some(ME),
            json!({"pending": 1}),
            3,
        )
        .await;
        t.store
            .update_document(doc(3), json!({"pending": 2}))
            .await
            .unwrap();
        seed_synced(
            &t.store,
            doc(4),
            SCOPE_CURATED,
            None,
            json!({"late": true}),
            40,
        )
        .await;
        seed_synced(
            &t.store,
            doc(5),
            SCOPE_CURATED,
            None,
            json!({"both": true}),
            3,
        )
        .await;
        apply(&t.store, doc(5), |_| {
            vec![DocOp::SetMembership(Membership {
                scope: SCOPE_OWN.into(),
                member: true,
                seq: 4,
            })]
        })
        .await;

        t.store
            .finish_snapshot(SCOPE_CURATED, &[doc(2)], 30)
            .await
            .unwrap();

        assert!(!snapshot(&t.store, doc(1)).await.exists, "missing: deleted");
        assert!(snapshot(&t.store, doc(2)).await.exists, "seen: kept");
        assert!(snapshot(&t.store, doc(3)).await.exists, "outbox rows: kept");
        assert!(
            snapshot(&t.store, doc(4)).await.exists,
            "newer membership: kept"
        );
        let both = snapshot(&t.store, doc(5)).await;
        assert!(both.exists, "member of own: kept");
        assert!(both.memberships.contains(&Membership {
            scope: SCOPE_CURATED.into(),
            member: false,
            seq: 30
        }));
    }

    #[tokio::test]
    async fn finish_snapshot_persists_snapshot_seq() {
        let t = temp_store().await;
        t.store
            .finish_snapshot(SCOPE_CURATED, &[], 42)
            .await
            .unwrap();
        assert_eq!(cursor(&t, SCOPE_CURATED).await, 42);
        t.store
            .finish_snapshot(SCOPE_CURATED, &[], 10)
            .await
            .unwrap();
        assert_eq!(cursor(&t, SCOPE_CURATED).await, 42);
    }

    #[tokio::test]
    async fn drop_subscription_deletes_row() {
        let t = temp_store().await;
        t.store.drop_subscription(SCOPE_CURATED).await.unwrap();
        assert_eq!(
            t.store.subscribed_scopes().await.unwrap(),
            vec![SCOPE_OWN.to_string()]
        );
        assert_eq!(t.store.load_cursors().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn lagging_scope_cannot_recreate_a_deleted_document() {
        let t = temp_store().await;
        seed_synced(&t.store, doc(1), SCOPE_OWN, Some(ME), json!({"v": 1}), 5).await;
        t.store
            .apply_changes(ME, SCOPE_OWN, &[delete_change(SCOPE_OWN, doc(1), 10)], 10)
            .await
            .unwrap();
        let lagging = upsert_change(
            SCOPE_CURATED,
            envelope(doc(1), Some(ME), json!({"v": 1}), 8),
        );
        t.store
            .apply_changes(ME, SCOPE_CURATED, &[lagging], 8)
            .await
            .unwrap();
        let after = snapshot(&t.store, doc(1)).await;
        assert!(!after.exists);
        assert_eq!(after.tombstone_seq, Some(10));
    }

    #[tokio::test]
    async fn a_full_snapshot_page_leaves_host_writes_well_inside_the_busy_timeout() {
        let t = temp_store().await;
        let other = open_again(&t.path()).await;
        let items: Vec<u32> = (0..50).collect();
        let docs: Vec<_> = (0..500u128)
            .map(|n| {
                envelope(
                    doc(n),
                    Some(ME),
                    json!({"title": format!("t{n}"), "items": items}),
                    10,
                )
            })
            .collect();
        let bound = crate::store::BUSY_TIMEOUT / 2;
        let started = std::time::Instant::now();
        let (page, write) =
            tokio::join!(t.store.apply_snapshot_page(ME, SCOPE_OWN, &docs), async {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                let began = std::time::Instant::now();
                let created = other.create_document(None, json!({"n": 1})).await;
                (created, began.elapsed())
            });
        page.unwrap();
        let page_took = started.elapsed();
        let (created, waited) = write;
        created.unwrap();
        assert!(
            page_took < bound,
            "a 500-document page held the write lock for {page_took:?}"
        );
        assert!(
            waited < bound,
            "a host write in another process waited {waited:?} behind a snapshot page"
        );
    }

    #[tokio::test]
    async fn a_pending_delete_on_a_document_that_became_a_publication_leaves_it_visible() {
        let t = temp_store().await;
        seed_synced(
            &t.store,
            doc(1),
            SCOPE_OWN,
            Some(ME),
            json!({"title": "Mine"}),
            3,
        )
        .await;
        t.store.delete_document(doc(1)).await.unwrap();
        let mut publication = envelope(doc(1), Some(ME), json!({"title": "Mine"}), 4);
        publication.read_only = true;
        t.store
            .apply_changes(
                ME,
                SCOPE_CURATED,
                &[upsert_change(SCOPE_CURATED, publication)],
                4,
            )
            .await
            .unwrap();
        let visible = format!(
            "SELECT COUNT(*) FROM documents WHERE id = '{}' AND deleted_at IS NULL AND read_only = 1",
            doc(1)
        );
        assert_eq!(count(&t.store, &visible).await, 1);
        assert_eq!(count(&t.store, "SELECT COUNT(*) FROM outbox").await, 0);
        assert_eq!(
            count(&t.store, "SELECT COUNT(*) FROM recovered").await,
            0,
            "deleted without an edit: nothing kept"
        );
        let last_kind: String =
            sqlx::query_scalar("SELECT kind FROM change_log ORDER BY local_seq DESC LIMIT 1")
                .fetch_one(&t.store.pool)
                .await
                .unwrap();
        assert_eq!(last_kind, "upsert", "the host is told it is back");
    }
}
