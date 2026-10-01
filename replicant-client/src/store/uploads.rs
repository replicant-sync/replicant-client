//! Upload effects: pending documents, building one upload, settling its reply, server copies.

use uuid::Uuid;

use super::docs::{apply_ops, load_snapshot, LogOrigin};
use super::{DocNotice, Store, StoreError, StoreResult};
use crate::engine::doc::{self as rules, DocOp, DocSnapshot, InFlight};
use crate::engine::doc_upload::reply_diverged;
use crate::engine::machine::{BuildOutcome, SettleOutcome};
use crate::engine::types::{DocEnvelope, Seq, ServerError};

/// The documents the uploader will send. Every other document with outbox rows is parked.
pub(super) const PENDING_DOC_IDS: &str = "SELECT doc_id FROM outbox GROUP BY doc_id \
     HAVING SUM(parked_error IS NOT NULL) = 0 OR (SELECT kind = 'delete' AND parked_error IS NULL \
         FROM outbox last WHERE last.doc_id = outbox.doc_id ORDER BY mutation_id DESC LIMIT 1)";

impl Store {
    /// `Effect::LoadPending`: documents with outbox rows and none parked (or an unparked delete
    /// as the last row), oldest row first.
    pub async fn load_pending(&self) -> StoreResult<Vec<Uuid>> {
        let ids: Vec<String> =
            sqlx::query_scalar(&format!("{PENDING_DOC_IDS} ORDER BY MIN(mutation_id)"))
                .fetch_all(&self.pool)
                .await?;
        ids.iter()
            .map(|id| Uuid::parse_str(id).map_err(StoreError::from))
            .collect()
    }

    /// `Effect::BuildUpload`. Rows and content are read in one transaction, so every covered
    /// row's edit is in `sent_content`.
    pub async fn build_upload(&self, me: Uuid, doc_id: Uuid) -> StoreResult<BuildOutcome> {
        let mut tx = self.begin().await?;
        let snap = load_snapshot(&mut tx, doc_id).await?;
        let outcome = match rules::build_upload(&snap, me) {
            rules::BuildResult::Send { upload, inflight } => {
                BuildOutcome::Send { upload, inflight }
            }
            rules::BuildResult::NeedsServerCopy => BuildOutcome::NeedsServerCopy,
            rules::BuildResult::Nothing => BuildOutcome::Nothing,
            rules::BuildResult::SettleLocally(ops) => {
                apply_ops(
                    &mut tx,
                    &self.writer(LogOrigin::Server),
                    &snap,
                    &ops,
                    None,
                    self.title_pointer.as_deref(),
                )
                .await?;
                BuildOutcome::SettledLocally {
                    rows_remain: !snap.project(&ops).rows.is_empty(),
                }
            }
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// Marks the rows an upload covers as sent in it, before the upload can reach the server.
    /// The mark goes away with the row; an error reply leaves it, because an earlier send of
    /// the same rows may have landed.
    pub async fn mark_sent(&self, doc_id: Uuid, upload_id: Uuid) -> StoreResult<()> {
        sqlx::query("UPDATE outbox SET sent_upload_id = ? WHERE doc_id = ? AND mutation_id <= ?")
            .bind(upload_id.to_string())
            .bind(doc_id.to_string())
            .bind(upload_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// `Effect::SettleUpload`.
    pub async fn settle_upload(
        &self,
        me: Uuid,
        doc_id: Uuid,
        inflight: &InFlight,
        reply: &Result<DocEnvelope, ServerError>,
        mismatch_attempts: u32,
        divergent_replies: u32,
    ) -> StoreResult<(SettleOutcome, Vec<DocNotice>)> {
        let mut tx = self.begin().await?;
        let snap = load_snapshot(&mut tx, doc_id).await?;
        let diverged = reply_diverged(&snap, inflight, reply);
        if diverged {
            tracing::warn!(%doc_id, "the server's copy differs from what was uploaded");
        }
        let settled = match rules::settle(
            &snap,
            inflight,
            reply,
            me,
            mismatch_attempts,
            divergent_replies,
        ) {
            rules::SettleResult::Ops(ops) => {
                let writer = self.writer(LogOrigin::Server);
                let notices = apply_ops(
                    &mut tx,
                    &writer,
                    &snap,
                    &ops,
                    reply.as_ref().ok(),
                    self.title_pointer.as_deref(),
                )
                .await?;
                let rows_remain = !snap.project(&ops).rows.is_empty();
                (
                    SettleOutcome::Done {
                        rows_remain,
                        diverged,
                    },
                    notices,
                )
            }
            rules::SettleResult::FetchServerCopy => (SettleOutcome::FetchServerCopy, Vec::new()),
            rules::SettleResult::Retry { after_ms, mismatch } => {
                (SettleOutcome::Retry { after_ms, mismatch }, Vec::new())
            }
        };
        tx.commit().await?;
        Ok(settled)
    }

    /// `Effect::ApplyServerCopy`; `None` means `get_document` answered `not_found`.
    pub async fn apply_server_copy(
        &self,
        me: Uuid,
        doc_id: Uuid,
        doc: Option<&DocEnvelope>,
    ) -> StoreResult<Vec<DocNotice>> {
        self.apply_rule(doc_id, doc, |snap| match doc {
            Some(doc) => rules::apply_server_copy(snap, doc, me, &self.list_merge),
            None => rules::apply_server_missing(snap),
        })
        .await
    }

    /// `Effect::ApplyServerDeleted`: `get_document` reported the document deleted.
    pub async fn apply_server_deleted(
        &self,
        doc_id: Uuid,
        seq: Seq,
    ) -> StoreResult<Vec<DocNotice>> {
        self.apply_rule(doc_id, None, |snap| rules::apply_server_deleted(snap, seq))
            .await
    }

    async fn apply_rule(
        &self,
        doc_id: Uuid,
        envelope: Option<&DocEnvelope>,
        rule: impl FnOnce(&DocSnapshot) -> Vec<DocOp>,
    ) -> StoreResult<Vec<DocNotice>> {
        let mut tx = self.begin().await?;
        let snap = load_snapshot(&mut tx, doc_id).await?;
        let ops = rule(&snap);
        let notices = apply_ops(
            &mut tx,
            &self.writer(LogOrigin::Server),
            &snap,
            &ops,
            envelope,
            self.title_pointer.as_deref(),
        )
        .await?;
        tx.commit().await?;
        Ok(notices)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;
    use crate::engine::doc::fixtures::DOC;
    use crate::engine::doc::{DocEvent, RowKind};
    use crate::engine::types::{Upload, UploadKind, SCOPE_OWN};
    use crate::store::test_support::*;
    use crate::store::KeptCopy;

    async fn sent(store: &Store, doc_id: Uuid) -> (Upload, InFlight) {
        match store.build_upload(ME, doc_id).await.unwrap() {
            BuildOutcome::Send { upload, inflight } => (upload, inflight),
            other => panic!("expected an upload, got {other:?}"),
        }
    }

    async fn recovered(t: &TempStore) -> Vec<(Value, String)> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT content, reason FROM recovered ORDER BY id")
                .fetch_all(&t.store.pool)
                .await
                .unwrap();
        rows.into_iter()
            .map(|(content, reason)| (serde_json::from_str(&content).unwrap(), reason))
            .collect()
    }

    #[tokio::test]
    async fn lost_create_of_an_integral_float_adopts_the_servers_copy() {
        let t = temp_store().await;
        t.store
            .create_document(Some(DOC), json!({"n": 1200.0}))
            .await
            .unwrap();
        let (_, inflight) = sent(&t.store, DOC).await;
        t.store.mark_sent(DOC, inflight.upload_id).await.unwrap();
        // The create landed but its reply was lost; the resync reads the server's jsonb row.
        let server = envelope(DOC, Some(ME), json!({"n": 1200}), 4);
        let notices = t
            .store
            .apply_snapshot_page(ME, SCOPE_OWN, std::slice::from_ref(&server))
            .await
            .unwrap();
        assert_eq!(notices, vec![]);
        assert!(recovered(&t).await.is_empty());
        assert_eq!(count(&t.store, "SELECT COUNT(*) FROM outbox").await, 0);
        let shadow = snapshot(&t.store, DOC).await.shadow.unwrap();
        assert_eq!(shadow.seq, 4);
        assert_eq!(shadow.content, json!({"n": 1200}));
    }

    #[tokio::test]
    async fn load_pending_skips_parked_docs_and_lists_oldest_first() {
        let t = temp_store().await;
        let first = t
            .store
            .create_document(None, json!({"a": 1}))
            .await
            .unwrap();
        let parked = t
            .store
            .create_document(None, json!({"b": 1}))
            .await
            .unwrap();
        let last = t
            .store
            .create_document(None, json!({"c": 1}))
            .await
            .unwrap();
        sqlx::query("UPDATE outbox SET parked_error = 'validation' WHERE doc_id = ?")
            .bind(parked.to_string())
            .execute(&t.store.pool)
            .await
            .unwrap();
        assert_eq!(t.store.load_pending().await.unwrap(), vec![first, last]);
    }

    #[tokio::test]
    async fn load_pending_lists_a_parked_doc_whose_last_row_is_a_delete() {
        let t = temp_store().await;
        let doc_id = t
            .store
            .create_document(None, json!({"a": 1}))
            .await
            .unwrap();
        sqlx::query("UPDATE outbox SET parked_error = 'validation'")
            .execute(&t.store.pool)
            .await
            .unwrap();
        assert!(t.store.load_pending().await.unwrap().is_empty());
        sqlx::query(
            "INSERT INTO outbox (mutation_id, doc_id, kind, created_at) \
             VALUES ('ffffffff-ffff-ffff-ffff-ffffffffffff', ?, 'delete', 0)",
        )
        .bind(doc_id.to_string())
        .execute(&t.store.pool)
        .await
        .unwrap();
        assert_eq!(t.store.load_pending().await.unwrap(), vec![doc_id]);
        sqlx::query("UPDATE outbox SET parked_error = 'forbidden'")
            .execute(&t.store.pool)
            .await
            .unwrap();
        assert!(t.store.load_pending().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn mark_sent_marks_only_the_rows_the_upload_covers() {
        let t = temp_store().await;
        let doc_id = t
            .store
            .create_document(None, json!({"n": 1}))
            .await
            .unwrap();
        t.store
            .update_document(doc_id, json!({"n": 2}))
            .await
            .unwrap();
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT mutation_id FROM outbox WHERE doc_id = ? ORDER BY mutation_id",
        )
        .bind(doc_id.to_string())
        .fetch_all(&t.store.pool)
        .await
        .unwrap();
        let first = Uuid::parse_str(&rows[0]).unwrap();
        t.store.mark_sent(doc_id, first).await.unwrap();
        let marked: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT sent_upload_id FROM outbox WHERE doc_id = ? ORDER BY mutation_id",
        )
        .bind(doc_id.to_string())
        .fetch_all(&t.store.pool)
        .await
        .unwrap();
        assert_eq!(marked, vec![Some(first.to_string()), None]);
        assert_eq!(snapshot(&t.store, doc_id).await.unacked_upload, Some(first));
    }

    #[tokio::test]
    async fn ack_deletes_only_covered_rows_and_keeps_the_server_hash_verbatim() {
        let t = temp_store().await;
        seed_synced(&t.store, DOC, SCOPE_OWN, Some(ME), json!({"n": 0}), 1).await;
        t.store.update_document(DOC, json!({"n": 1})).await.unwrap();
        let (_, inflight) = sent(&t.store, DOC).await;
        t.store.update_document(DOC, json!({"n": 2})).await.unwrap();

        let mut reply = envelope(DOC, Some(ME), json!({"n": 1}), 2);
        reply.hash = "server-hash-2".into();
        let (outcome, notices) = t
            .store
            .settle_upload(ME, DOC, &inflight, &Ok(reply), 0, 0)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            SettleOutcome::Done {
                rows_remain: true,
                diverged: false
            }
        );
        assert!(notices.is_empty());
        let after = snapshot(&t.store, DOC).await;
        assert_eq!(after.rows.len(), 1);
        assert!(after.rows[0].mutation_id > inflight.upload_id);
        assert_eq!(after.shadow.as_ref().unwrap().hash, "server-hash-2");
        let (upload, _) = sent(&t.store, DOC).await;
        assert_eq!(upload.kind, UploadKind::Update);
        assert_eq!(upload.base_hash.as_deref(), Some("server-hash-2"));
    }

    #[tokio::test]
    async fn settling_the_same_reply_twice_is_a_noop_the_second_time() {
        let t = temp_store().await;
        seed_synced(&t.store, DOC, SCOPE_OWN, Some(ME), json!({"n": 0}), 1).await;
        t.store.update_document(DOC, json!({"n": 1})).await.unwrap();
        let (_, inflight) = sent(&t.store, DOC).await;

        let mut reply = envelope(DOC, Some(ME), json!({"n": 1}), 2);
        reply.hash = "server-hash-2".into();

        let (first_outcome, _) = t
            .store
            .settle_upload(ME, DOC, &inflight, &Ok(reply.clone()), 0, 0)
            .await
            .unwrap();
        assert_eq!(
            first_outcome,
            SettleOutcome::Done {
                rows_remain: false,
                diverged: false
            }
        );
        let before = snapshot(&t.store, DOC).await;

        // Two processes (or an ack retried after a dropped ack) settling the same upload_id
        // with the same reply: the second settle must be a no-op, not a second write.
        let (second_outcome, second_notices) = t
            .store
            .settle_upload(ME, DOC, &inflight, &Ok(reply), 0, 0)
            .await
            .unwrap();

        assert_eq!(
            second_outcome,
            SettleOutcome::Done {
                rows_remain: false,
                diverged: false
            }
        );
        assert!(second_notices.is_empty());
        let after = snapshot(&t.store, DOC).await;
        assert_eq!(after.shadow, before.shadow);
        assert!(after.rows.is_empty());
    }

    #[tokio::test]
    async fn build_is_stable_across_reopen() {
        let t = temp_store().await;
        let doc_id = t
            .store
            .create_document(None, json!({"title": "Offline"}))
            .await
            .unwrap();
        let first = t.store.build_upload(ME, doc_id).await.unwrap();
        t.store.close().await;
        let reopened = Store::open(&t.path()).await.unwrap();
        assert_eq!(reopened.build_upload(ME, doc_id).await.unwrap(), first);
    }

    #[tokio::test]
    async fn empty_diff_settles_locally() {
        let t = temp_store().await;
        seed_synced(&t.store, DOC, SCOPE_OWN, Some(ME), json!({"n": 0}), 1).await;
        t.store.update_document(DOC, json!({"n": 0})).await.unwrap();
        assert_eq!(
            t.store.build_upload(ME, DOC).await.unwrap(),
            BuildOutcome::SettledLocally { rows_remain: false }
        );
        assert!(snapshot(&t.store, DOC).await.rows.is_empty());
    }

    #[tokio::test]
    async fn unsent_edit_whose_row_was_lost_gets_a_new_marker() {
        let t = temp_store().await;
        seed_synced(&t.store, DOC, SCOPE_OWN, Some(ME), json!({"n": 0}), 1).await;
        t.store.update_document(DOC, json!({"n": 1})).await.unwrap();
        let (_, inflight) = sent(&t.store, DOC).await;
        let other = open_again(&t.path()).await;
        other.update_document(DOC, json!({"n": 2})).await.unwrap();
        // The other process settled rows up to an echo's upload_id; UUIDv7 order across
        // processes is approximate, so its own unsent row went too.
        sqlx::query("DELETE FROM outbox WHERE doc_id = ?")
            .bind(DOC.to_string())
            .execute(&other.pool)
            .await
            .unwrap();

        let reply = envelope(DOC, Some(ME), json!({"n": 1}), 2);
        let (outcome, _) = t
            .store
            .settle_upload(ME, DOC, &inflight, &Ok(reply.clone()), 0, 0)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            SettleOutcome::Done {
                rows_remain: true,
                diverged: false
            }
        );
        let (upload, _) = sent(&t.store, DOC).await;
        assert_eq!(upload.base_hash, Some(reply.hash));
        assert_eq!(
            upload.payload,
            json!([{"op": "replace", "path": "/n", "value": 2}])
        );
    }

    #[tokio::test]
    async fn hash_drift_does_not_reinsert_a_marker() {
        let t = temp_store().await;
        seed_synced(&t.store, DOC, SCOPE_OWN, Some(ME), json!({"f": 1}), 1).await;
        t.store.update_document(DOC, json!({"f": 2})).await.unwrap();
        let (_, inflight) = sent(&t.store, DOC).await;
        let mut reply = envelope(DOC, Some(ME), json!({"f": 2}), 2);
        reply.hash = "jsonb-normalised-hash".into();
        let (outcome, _) = t
            .store
            .settle_upload(ME, DOC, &inflight, &Ok(reply), 0, 0)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            SettleOutcome::Done {
                rows_remain: false,
                diverged: false
            }
        );
        assert!(snapshot(&t.store, DOC).await.rows.is_empty());
        assert!(t.store.load_pending().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn conflict_settle_keeps_local_content_in_recovered() {
        let t = temp_store().await;
        seed_synced(&t.store, DOC, SCOPE_OWN, Some(ME), json!({"n": 0}), 1).await;
        let local = json!({"n": 1, "mine": true});
        t.store.update_document(DOC, local.clone()).await.unwrap();
        let (_, inflight) = sent(&t.store, DOC).await;
        let mut mismatch = ServerError::new("hash_mismatch");
        mismatch.current_hash = Some("elsewhere".into());
        mismatch.current_seq = Some(9);

        let (outcome, notices) = t
            .store
            .settle_upload(ME, DOC, &inflight, &Err(mismatch), 3, 0)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            SettleOutcome::Done {
                rows_remain: false,
                diverged: false
            }
        );
        assert_eq!(
            notices,
            vec![DocNotice {
                doc_id: DOC,
                event: DocEvent::ConflictDetected,
                kept: Some(KeptCopy {
                    recovered_id: 1,
                    reason: "conflict".into()
                })
            }]
        );
        assert_eq!(recovered(&t).await, vec![(local, "conflict".to_string())]);
        assert_eq!(snapshot(&t.store, DOC).await.content, json!({"n": 0}));
    }

    #[tokio::test]
    async fn validation_error_leaves_the_sent_mark_and_a_later_snapshot_still_conflicts() {
        let t = temp_store().await;
        seed_synced(&t.store, DOC, SCOPE_OWN, Some(ME), json!({"n": 0}), 1).await;
        t.store.update_document(DOC, json!({"n": 1})).await.unwrap();
        let (_, inflight) = sent(&t.store, DOC).await;
        t.store.mark_sent(DOC, inflight.upload_id).await.unwrap();

        let (outcome, notices) = t
            .store
            .settle_upload(
                ME,
                DOC,
                &inflight,
                &Err(ServerError::new("validation")),
                0,
                0,
            )
            .await
            .unwrap();

        assert_eq!(
            outcome,
            SettleOutcome::Done {
                rows_remain: true,
                diverged: false
            }
        );
        assert_eq!(
            notices,
            vec![DocNotice {
                doc_id: DOC,
                event: DocEvent::SyncError {
                    code: "validation".into()
                },
                kept: None
            }]
        );
        // An error reply does not clear the mark: an earlier send of the same rows may have
        // landed, and only the row's eventual settlement proves which version did.
        let marked: Option<String> =
            sqlx::query_scalar("SELECT sent_upload_id FROM outbox WHERE doc_id = ?")
                .bind(DOC.to_string())
                .fetch_one(&t.store.pool)
                .await
                .unwrap();
        assert_eq!(marked, Some(inflight.upload_id.to_string()));

        // A later snapshot with a changed server doc must still take the conflict path for the
        // parked, still-marked row: it may already be applied there.
        let server = envelope(DOC, Some(ME), json!({"n": 9}), 5);
        let notices = t
            .store
            .apply_snapshot_page(ME, SCOPE_OWN, std::slice::from_ref(&server))
            .await
            .unwrap();
        assert_eq!(
            notices,
            vec![DocNotice {
                doc_id: DOC,
                event: DocEvent::ConflictDetected,
                kept: Some(KeptCopy {
                    recovered_id: 1,
                    reason: "conflict".into()
                })
            }]
        );
    }

    #[tokio::test]
    async fn server_deleted_takes_delete_wins_path() {
        let t = temp_store().await;
        seed_synced(&t.store, DOC, SCOPE_OWN, Some(ME), json!({"n": 0}), 1).await;
        t.store.update_document(DOC, json!({"n": 5})).await.unwrap();

        let notices = t.store.apply_server_deleted(DOC, 7).await.unwrap();

        assert_eq!(
            notices,
            vec![DocNotice {
                doc_id: DOC,
                event: DocEvent::ConflictDetected,
                kept: Some(KeptCopy {
                    recovered_id: 1,
                    reason: "delete_wins".into()
                })
            }]
        );
        assert_eq!(
            recovered(&t).await,
            vec![(json!({"n": 5}), "delete_wins".to_string())]
        );
        let after = snapshot(&t.store, DOC).await;
        assert!(!after.exists);
        assert!(after.rows.is_empty());
        assert_eq!(after.tombstone_seq, Some(7));
    }

    #[tokio::test]
    async fn server_missing_turns_rows_into_a_create() {
        let t = temp_store().await;
        // A migrated document: pending edit rows and no shadow.
        apply(&t.store, DOC, |_| {
            vec![
                DocOp::SetMeta {
                    owner_id: Some(ME),
                    read_only: false,
                },
                DocOp::SetContent(json!({"n": 3})),
                DocOp::InsertMarker(RowKind::Update),
            ]
        })
        .await;
        assert_eq!(
            t.store.build_upload(ME, DOC).await.unwrap(),
            BuildOutcome::NeedsServerCopy
        );
        t.store.apply_server_copy(ME, DOC, None).await.unwrap();
        let (upload, _) = sent(&t.store, DOC).await;
        assert_eq!(upload.kind, UploadKind::Create);
        assert_eq!(upload.payload, json!({"n": 3}));
    }

    #[tokio::test]
    async fn server_copy_on_a_migrated_document_with_pending_rows_takes_the_conflict_path() {
        let t = temp_store().await;
        apply(&t.store, DOC, |_| {
            vec![
                DocOp::SetMeta {
                    owner_id: Some(ME),
                    read_only: false,
                },
                DocOp::SetContent(json!({"n": 3})),
                DocOp::InsertMarker(RowKind::Update),
            ]
        })
        .await;
        let server = envelope(DOC, Some(ME), json!({"n": 1}), 4);
        let notices = t
            .store
            .apply_server_copy(ME, DOC, Some(&server))
            .await
            .unwrap();

        assert_eq!(
            notices,
            vec![DocNotice {
                doc_id: DOC,
                event: DocEvent::ConflictDetected,
                kept: Some(KeptCopy {
                    recovered_id: 1,
                    reason: "conflict".into()
                })
            }]
        );
        assert_eq!(
            recovered(&t).await,
            vec![(json!({"n": 3}), "conflict".to_string())]
        );
        let after = snapshot(&t.store, DOC).await;
        assert_eq!(after.content, json!({"n": 1}));
        assert!(after.rows.is_empty());
        assert_eq!(after.shadow.as_ref().unwrap().hash, server.hash);
    }

    #[tokio::test]
    async fn a_refused_resend_keeps_its_mark_so_a_snapshot_cannot_resurrect_a_removed_element() {
        let t = temp_store().await;
        seed_synced(
            &t.store,
            DOC,
            SCOPE_OWN,
            Some(ME),
            json!({"items": ["a"], "n": 0}),
            1,
        )
        .await;
        // The first upload appends x and lands, but its reply and echo are never seen.
        t.store
            .update_document(DOC, json!({"items": ["a", "x"], "n": 0}))
            .await
            .unwrap();
        let (_, first) = sent(&t.store, DOC).await;
        t.store.mark_sent(DOC, first.upload_id).await.unwrap();
        // A later edit; the resend covers both rows on the stale base and is refused.
        let local = json!({"items": ["a", "x"], "n": 1});
        t.store.update_document(DOC, local.clone()).await.unwrap();
        let (_, second) = sent(&t.store, DOC).await;
        t.store.mark_sent(DOC, second.upload_id).await.unwrap();
        let refused = ServerError {
            current_hash: Some("landed".into()),
            current_seq: Some(4),
            ..ServerError::new("hash_mismatch")
        };
        t.store
            .settle_upload(ME, DOC, &second, &Err(refused), 0, 0)
            .await
            .unwrap();

        // Another device removed x; a full resync snapshot arrives.
        let server = envelope(DOC, Some(ME), json!({"items": ["a"], "n": 0}), 6);
        let notices = t
            .store
            .apply_snapshot_page(ME, SCOPE_OWN, std::slice::from_ref(&server))
            .await
            .unwrap();

        assert_eq!(
            notices,
            vec![DocNotice {
                doc_id: DOC,
                event: DocEvent::ConflictDetected,
                kept: Some(KeptCopy {
                    recovered_id: 1,
                    reason: "conflict".into()
                })
            }]
        );
        assert_eq!(recovered(&t).await, vec![(local, "conflict".to_string())]);
        assert_eq!(
            snapshot(&t.store, DOC).await.content,
            json!({"items": ["a"], "n": 0}),
            "x is not silently brought back"
        );
    }
}
