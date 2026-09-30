//! Loading a document snapshot and writing a rule's `DocOp`s inside the caller's transaction.

use serde_json::Value;
use sqlx::{Row, SqliteConnection};
use uuid::Uuid;

use super::{now_rfc3339, now_unix, DocNotice, KeptCopy, StoreError, StoreResult};
use crate::engine::doc::{
    DocOp, DocSnapshot, Membership, OutboxRow, RecoverReason, RowKind, Shadow,
};
use crate::engine::hash::content_hash;
use crate::engine::types::DocEnvelope;
use crate::queries::Queries;

const TITLE_MAX_CHARS: usize = 128;

/// Change-log `origin`: this data dir's host, or anything the sync engine wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogOrigin {
    Local,
    Server,
}

impl LogOrigin {
    fn as_str(self) -> &'static str {
        match self {
            LogOrigin::Local => "local",
            LogOrigin::Server => "server",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Writer {
    pub instance_id: Uuid,
    pub origin: LogOrigin,
}

pub(crate) async fn load_snapshot(
    conn: &mut SqliteConnection,
    doc_id: Uuid,
) -> StoreResult<DocSnapshot> {
    let id = doc_id.to_string();
    let mut snap = DocSnapshot {
        doc_id,
        ..Default::default()
    };
    let row = sqlx::query(
        "SELECT content, user_id, read_only, deleted_at, server_content, server_hash, server_seq \
         FROM documents WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(row) = row {
        snap.exists = true;
        snap.content = serde_json::from_str(&row.try_get::<String, _>("content")?)?;
        snap.owner_id = row
            .try_get::<Option<String>, _>("user_id")?
            .map(|owner| Uuid::parse_str(&owner))
            .transpose()?;
        snap.read_only = row.try_get::<i64, _>("read_only")? != 0;
        snap.soft_deleted = row.try_get::<Option<String>, _>("deleted_at")?.is_some();
        if let Some(server_content) = row.try_get::<Option<String>, _>("server_content")? {
            let hash: String = row
                .try_get::<Option<String>, _>("server_hash")?
                .ok_or_else(|| {
                    StoreError::Corrupt(format!(
                        "document {doc_id}: server_content without server_hash"
                    ))
                })?;
            snap.shadow = Some(Shadow {
                content: serde_json::from_str(&server_content)?,
                hash,
                seq: row
                    .try_get::<Option<i64>, _>("server_seq")?
                    .ok_or_else(|| {
                        StoreError::Corrupt(format!(
                            "document {doc_id}: server_content without server_seq"
                        ))
                    })?,
            });
        }
    }

    let rows = sqlx::query(
        "SELECT mutation_id, kind, parked_error, sent_upload_id FROM outbox WHERE doc_id = ?",
    )
    .bind(&id)
    .fetch_all(&mut *conn)
    .await?;
    for row in rows {
        if let Some(sent) = row.try_get::<Option<String>, _>("sent_upload_id")? {
            snap.unacked_upload = snap.unacked_upload.max(Some(Uuid::parse_str(&sent)?));
        }
        snap.rows.push(OutboxRow {
            mutation_id: Uuid::parse_str(&row.try_get::<String, _>("mutation_id")?)?,
            kind: parse_row_kind(&row.try_get::<String, _>("kind")?)?,
            parked: row.try_get::<Option<String>, _>("parked_error")?.is_some(),
        });
    }
    snap.rows.sort_by_key(|r| r.mutation_id);

    let memberships =
        sqlx::query("SELECT scope, member, seq FROM doc_scopes WHERE doc_id = ? ORDER BY scope")
            .bind(&id)
            .fetch_all(&mut *conn)
            .await?;
    for row in memberships {
        snap.memberships.push(Membership {
            scope: row.try_get("scope")?,
            member: row.try_get::<i64, _>("member")? != 0,
            seq: row.try_get("seq")?,
        });
    }

    snap.tombstone_seq =
        sqlx::query_scalar::<_, i64>("SELECT server_seq FROM tombstones WHERE doc_id = ?")
            .bind(&id)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(snap)
}

/// Writes `ops` for `snap.doc_id`. The document row is written once, from the projection, so
/// op order within the row fields does not matter; outbox and membership ops run in order.
pub(crate) async fn apply_ops(
    conn: &mut SqliteConnection,
    writer: &Writer,
    snap: &DocSnapshot,
    ops: &[DocOp],
    envelope: Option<&DocEnvelope>,
) -> StoreResult<Vec<DocNotice>> {
    let id = snap.doc_id.to_string();
    let mut notices = Vec::new();
    let mut kept: Option<KeptCopy> = None;
    for op in ops {
        match op {
            DocOp::DeleteRows(mutation_ids) => {
                for mutation_id in mutation_ids {
                    sqlx::query("DELETE FROM outbox WHERE doc_id = ? AND mutation_id = ?")
                        .bind(&id)
                        .bind(mutation_id.to_string())
                        .execute(&mut *conn)
                        .await?;
                }
            }
            DocOp::DropAllRows => {
                sqlx::query("DELETE FROM outbox WHERE doc_id = ?")
                    .bind(&id)
                    .execute(&mut *conn)
                    .await?;
            }
            DocOp::InsertMarker(kind) => {
                insert_marker(&mut *conn, snap.doc_id, *kind).await?;
            }
            DocOp::Park { rows, error } => {
                for mutation_id in rows {
                    sqlx::query(
                        "UPDATE outbox SET parked_error = ? WHERE doc_id = ? AND mutation_id = ?",
                    )
                    .bind(error.as_str())
                    .bind(&id)
                    .bind(mutation_id.to_string())
                    .execute(&mut *conn)
                    .await?;
                }
            }
            DocOp::SetMembership(membership) => {
                sqlx::query(
                    "INSERT INTO doc_scopes (doc_id, scope, member, seq) VALUES (?, ?, ?, ?) \
                     ON CONFLICT(doc_id, scope) DO UPDATE SET member = excluded.member, seq = excluded.seq",
                )
                .bind(&id)
                .bind(membership.scope.as_str())
                .bind(membership.member as i64)
                .bind(membership.seq)
                .execute(&mut *conn)
                .await?;
            }
            DocOp::RecordTombstone(seq) => {
                sqlx::query(
                    "INSERT INTO tombstones (doc_id, server_seq, deleted_at) VALUES (?, ?, ?) \
                     ON CONFLICT(doc_id) DO UPDATE SET \
                     server_seq = max(server_seq, excluded.server_seq), deleted_at = excluded.deleted_at",
                )
                .bind(&id)
                .bind(*seq)
                .bind(now_unix())
                .execute(&mut *conn)
                .await?;
            }
            DocOp::Recover { content, reason } => {
                let inserted = sqlx::query(
                    "INSERT INTO recovered (doc_id, content, reason, recovered_at) VALUES (?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(content.to_string())
                .bind(reason_str(*reason))
                .bind(now_unix())
                .execute(&mut *conn)
                .await?;
                kept = Some(KeptCopy {
                    recovered_id: inserted.last_insert_rowid(),
                    reason: reason_str(*reason).to_string(),
                });
            }
            DocOp::RecoverFields { content, fields } => {
                let inserted = sqlx::query(
                    "INSERT INTO recovered (doc_id, content, reason, recovered_at, fields) \
                     VALUES (?, ?, 'field_conflict', ?, ?)",
                )
                .bind(&id)
                .bind(content.to_string())
                .bind(now_unix())
                .bind(serde_json::to_string(fields)?)
                .execute(&mut *conn)
                .await?;
                kept = Some(KeptCopy {
                    recovered_id: inserted.last_insert_rowid(),
                    reason: "field_conflict".to_string(),
                });
            }
            DocOp::Emit(event) => notices.push(DocNotice {
                doc_id: snap.doc_id,
                event: event.clone(),
                kept: kept.clone(),
            }),
            DocOp::SetShadow(_)
            | DocOp::SetContent(_)
            | DocOp::SetMeta { .. }
            | DocOp::HardDelete
            | DocOp::Undelete => {}
        }
    }
    let after = snap.project(ops);
    write_document(&mut *conn, snap, &after, envelope).await?;
    if visible_change(snap, &after) {
        append_change_log(&mut *conn, writer, snap.doc_id, !after.exists).await?;
    }
    Ok(notices)
}

async fn write_document(
    conn: &mut SqliteConnection,
    before: &DocSnapshot,
    after: &DocSnapshot,
    envelope: Option<&DocEnvelope>,
) -> StoreResult<()> {
    let id = before.doc_id.to_string();
    if !after.exists {
        if before.exists {
            sqlx::query("DELETE FROM documents WHERE id = ?")
                .bind(&id)
                .execute(&mut *conn)
                .await?;
            refresh_search(&mut *conn, before.doc_id).await?;
        }
        return Ok(());
    }
    let content_changed = !before.exists || before.content != after.content;
    let row_changed = content_changed
        || before.owner_id != after.owner_id
        || before.read_only != after.read_only
        || before.shadow != after.shadow;
    if row_changed {
        let now = now_rfc3339();
        let shadow = after.shadow.as_ref();
        sqlx::query(
            "INSERT INTO documents (id, user_id, content, hash, title, server_content, server_hash, \
             server_seq, read_only, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET user_id = excluded.user_id, content = excluded.content, \
             hash = excluded.hash, title = COALESCE(excluded.title, documents.title), \
             server_content = excluded.server_content, server_hash = excluded.server_hash, \
             server_seq = excluded.server_seq, read_only = excluded.read_only, \
             updated_at = CASE WHEN documents.content = excluded.content \
             THEN documents.updated_at ELSE excluded.updated_at END",
        )
        .bind(&id)
        .bind(after.owner_id.map(|owner| owner.to_string()))
        .bind(after.content.to_string())
        .bind(content_hash(&after.content))
        .bind(title_of(&after.content, envelope))
        .bind(shadow.map(|s| s.content.to_string()))
        .bind(shadow.map(|s| s.hash.clone()))
        .bind(shadow.map(|s| s.seq))
        .bind(after.read_only as i64)
        .bind(&now)
        .bind(&now)
        .execute(&mut *conn)
        .await?;
        if content_changed {
            refresh_search(&mut *conn, before.doc_id).await?;
        }
    }
    if before.soft_deleted && !after.soft_deleted {
        sqlx::query("UPDATE documents SET deleted_at = NULL WHERE id = ?")
            .bind(&id)
            .execute(&mut *conn)
            .await?;
        refresh_search(&mut *conn, before.doc_id).await?;
    }
    // The envelope's version became the shadow: keep its provenance metadata.
    let applied = envelope.filter(|env| {
        before.shadow != after.shadow && after.shadow.as_ref().is_some_and(|s| s.seq == env.seq)
    });
    if let Some(env) = applied {
        sqlx::query(
            "UPDATE documents SET author_id = ?, source_doc_id = ?, derived_from = ? WHERE id = ?",
        )
        .bind(env.author_id.map(|u| u.to_string()))
        .bind(env.source_doc_id.map(|u| u.to_string()))
        .bind(env.derived_from.map(|u| u.to_string()))
        .bind(&id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

fn visible_change(before: &DocSnapshot, after: &DocSnapshot) -> bool {
    before.exists != after.exists
        || (after.exists
            && (before.content != after.content
                || before.owner_id != after.owner_id
                || before.read_only != after.read_only
                || before.soft_deleted != after.soft_deleted))
}

pub(crate) async fn insert_marker(
    conn: &mut SqliteConnection,
    doc_id: Uuid,
    kind: RowKind,
) -> StoreResult<Uuid> {
    let id = doc_id.to_string();
    let max: Option<String> = sqlx::query_scalar(
        "SELECT mutation_id FROM outbox WHERE doc_id = ? ORDER BY mutation_id DESC LIMIT 1",
    )
    .bind(&id)
    .fetch_optional(&mut *conn)
    .await?;
    let max = max.map(|m| Uuid::parse_str(&m)).transpose()?;
    let candidate = Uuid::now_v7();
    // Two Stores on one file can race or a clock can step backwards: per-doc ids must stay
    // strictly increasing, so a non-advancing candidate is bumped past the max instead
    // (it stops looking like a real v7 timestamp, but strict ordering is what callers need).
    let mutation_id = match max {
        Some(max) if candidate <= max => Uuid::from_u128(max.as_u128() + 1),
        _ => candidate,
    };
    sqlx::query("INSERT INTO outbox (mutation_id, doc_id, kind, created_at) VALUES (?, ?, ?, ?)")
        .bind(mutation_id.to_string())
        .bind(&id)
        .bind(row_kind_str(kind))
        .bind(now_unix())
        .execute(&mut *conn)
        .await?;
    Ok(mutation_id)
}

pub(crate) async fn append_change_log(
    conn: &mut SqliteConnection,
    writer: &Writer,
    doc_id: Uuid,
    deleted: bool,
) -> StoreResult<()> {
    sqlx::query(
        "INSERT INTO change_log (doc_id, kind, origin_instance, origin) VALUES (?, ?, ?, ?)",
    )
    .bind(doc_id.to_string())
    .bind(if deleted { "delete" } else { "upsert" })
    .bind(writer.instance_id.to_string())
    .bind(writer.origin.as_str())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Keeps the v1 full-text index in step when search paths are configured.
pub(crate) async fn refresh_search(conn: &mut SqliteConnection, doc_id: Uuid) -> StoreResult<()> {
    let configured: i64 = sqlx::query_scalar(Queries::HAS_SEARCH_CONFIG)
        .fetch_one(&mut *conn)
        .await?;
    if configured == 0 {
        return Ok(());
    }
    sqlx::query(Queries::DELETE_FTS_ENTRY)
        .bind(doc_id.to_string())
        .execute(&mut *conn)
        .await?;
    sqlx::query(Queries::UPDATE_FTS_ENTRY)
        .bind(doc_id.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub(crate) fn title_of(content: &Value, envelope: Option<&DocEnvelope>) -> Option<String> {
    content
        .get("title")
        .and_then(Value::as_str)
        .map(|title| title.chars().take(TITLE_MAX_CHARS).collect())
        .or_else(|| envelope.and_then(|env| env.title.clone()))
}

fn row_kind_str(kind: RowKind) -> &'static str {
    match kind {
        RowKind::Create => "create",
        RowKind::Update => "update",
        RowKind::Delete => "delete",
    }
}

fn parse_row_kind(text: &str) -> StoreResult<RowKind> {
    match text {
        "create" => Ok(RowKind::Create),
        "update" => Ok(RowKind::Update),
        "delete" => Ok(RowKind::Delete),
        other => Err(StoreError::Corrupt(format!("outbox kind {other}"))),
    }
}

fn reason_str(reason: RecoverReason) -> &'static str {
    match reason {
        RecoverReason::DeleteWins => "delete_wins",
        RecoverReason::Conflict => "conflict",
        RecoverReason::BecamePublication => "became_publication",
        RecoverReason::CreateRejected => "create_rejected",
        RecoverReason::DeleteSuperseded => "delete_superseded",
        RecoverReason::DeleteRefused => "delete_refused",
        RecoverReason::DeletePublication => "delete_publication",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;
    use crate::engine::doc::change_fixtures::{env, upsert, with_upload};
    use crate::engine::doc::fixtures::{synced, APPEND, DOC};
    use crate::engine::doc::{apply_change, DocEvent};
    use crate::engine::types::{SCOPE_CURATED, SCOPE_OWN};
    use crate::store::test_support::*;

    #[tokio::test]
    async fn ops_round_trip_through_the_database() {
        let t = temp_store().await;
        let content = json!({"title": "A", "n": 1});
        seed_synced(&t.store, DOC, SCOPE_OWN, Some(ME), content.clone(), 3).await;
        assert_eq!(snapshot(&t.store, DOC).await, synced(content, 3));
    }

    #[tokio::test]
    async fn a_kept_copy_is_named_in_the_notice_that_reports_it() {
        let t = temp_store().await;
        seed_synced(&t.store, DOC, "own", Some(ME), json!({"a": 1}), 1).await;
        let notices = apply(&t.store, DOC, |_| {
            vec![
                DocOp::Emit(DocEvent::SyncError {
                    code: "validation".into(),
                }),
                DocOp::Recover {
                    content: json!({"a": 2}),
                    reason: RecoverReason::DeleteWins,
                },
                DocOp::Emit(DocEvent::ConflictDetected),
            ]
        })
        .await;
        assert_eq!(
            notices[0].kept, None,
            "no copy was written before this notice"
        );
        let kept = notices[1]
            .kept
            .clone()
            .expect("the copy written just before it");
        assert_eq!(kept.reason, "delete_wins");
        assert_eq!(
            t.store.list_recovered().await.unwrap()[0].id,
            kept.recovered_id
        );
    }

    #[tokio::test]
    async fn superseded_and_refused_deletes_name_their_kept_copy() {
        let t = temp_store().await;
        seed_synced(&t.store, DOC, "own", Some(ME), json!({"a": 1}), 1).await;
        let notices = apply(&t.store, DOC, |_| {
            vec![
                DocOp::Recover {
                    content: json!({"a": 2}),
                    reason: RecoverReason::DeleteSuperseded,
                },
                DocOp::Emit(DocEvent::DeleteSuperseded),
                DocOp::Recover {
                    content: json!({"a": 3}),
                    reason: RecoverReason::DeleteRefused,
                },
                DocOp::Emit(DocEvent::SyncError {
                    code: "forbidden".into(),
                }),
            ]
        })
        .await;
        let reasons: Vec<Option<String>> = notices
            .iter()
            .map(|notice| notice.kept.as_ref().map(|kept| kept.reason.clone()))
            .collect();
        assert_eq!(
            reasons,
            vec![
                Some("delete_superseded".to_string()),
                Some("delete_refused".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn recover_keeps_local_content_in_recovered() {
        let t = temp_store().await;
        seed_synced(&t.store, DOC, SCOPE_OWN, Some(ME), json!({"a": 1}), 1).await;
        let local = json!({"a": 1, "mine": true, "later": true});
        let edited = local.clone();
        apply(&t.store, DOC, move |_| {
            vec![
                DocOp::SetContent(edited),
                DocOp::InsertMarker(RowKind::Update),
                DocOp::InsertMarker(RowKind::Update),
            ]
        })
        .await;
        let uploaded = snapshot(&t.store, DOC).await.rows[0].mutation_id;
        // The page row for the first upload carries a newer envelope while a later edit is
        // still pending: the rules take the conflict path and must keep the local content.
        let mut page_echo = with_upload(upsert(SCOPE_OWN, json!({}), 2), uploaded);
        page_echo.doc = Some(env(json!({"a": 1, "mine": true, "theirs": true}), 4));

        let notices = apply(&t.store, DOC, |snap| {
            apply_change(snap, &page_echo, ME, &APPEND)
        })
        .await;

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
        let recovered: Vec<(String, String)> =
            sqlx::query_as("SELECT content, reason FROM recovered WHERE doc_id = ?")
                .bind(DOC.to_string())
                .fetch_all(&t.store.pool)
                .await
                .unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(&recovered[0].0).unwrap(),
            local
        );
        assert_eq!(recovered[0].1, "conflict");
        let after = snapshot(&t.store, DOC).await;
        assert_eq!(after.content, json!({"a": 1, "mine": true, "theirs": true}));
        assert!(after.rows.is_empty());
    }

    #[tokio::test]
    async fn a_refused_delete_copy_is_listed_as_delete_refused() {
        let t = temp_store().await;
        seed_synced(&t.store, DOC, SCOPE_OWN, Some(ME), json!({"a": 1}), 1).await;
        apply(&t.store, DOC, |_| {
            vec![DocOp::Recover {
                content: json!({"a": 2}),
                reason: RecoverReason::DeleteRefused,
            }]
        })
        .await;

        let copies = t.store.list_recovered().await.unwrap();
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].reason, "delete_refused");
        assert_eq!(copies[0].content, json!({"a": 2}));
    }

    #[tokio::test]
    async fn hard_delete_keeps_membership_rows() {
        let t = temp_store().await;
        seed_synced(&t.store, DOC, SCOPE_OWN, Some(ME), json!({"a": 1}), 3).await;
        let left = Membership {
            scope: SCOPE_OWN.into(),
            member: false,
            seq: 9,
        };
        let op_left = left.clone();
        apply(&t.store, DOC, move |_| {
            vec![
                DocOp::SetMembership(op_left),
                DocOp::DropAllRows,
                DocOp::HardDelete,
                DocOp::RecordTombstone(9),
            ]
        })
        .await;
        let after = snapshot(&t.store, DOC).await;
        assert!(!after.exists);
        assert_eq!(after.memberships, vec![left]);
        assert_eq!(after.tombstone_seq, Some(9));
        assert_eq!(count(&t.store, "SELECT COUNT(*) FROM documents").await, 0);
    }

    #[tokio::test]
    async fn membership_without_a_document_loads() {
        let t = temp_store().await;
        let membership = Membership {
            scope: SCOPE_CURATED.into(),
            member: false,
            seq: 4,
        };
        let op_membership = membership.clone();
        apply(&t.store, DOC, move |_| {
            vec![DocOp::SetMembership(op_membership)]
        })
        .await;
        let loaded = snapshot(&t.store, DOC).await;
        assert!(!loaded.exists);
        assert_eq!(loaded.memberships, vec![membership]);
        assert_eq!(count(&t.store, "SELECT COUNT(*) FROM change_log").await, 0);
    }

    #[tokio::test]
    async fn tombstone_keeps_the_highest_seq() {
        let t = temp_store().await;
        apply(&t.store, DOC, |_| vec![DocOp::RecordTombstone(9)]).await;
        apply(&t.store, DOC, |_| vec![DocOp::RecordTombstone(4)]).await;
        assert_eq!(snapshot(&t.store, DOC).await.tombstone_seq, Some(9));
    }

    #[tokio::test]
    async fn markers_get_fresh_ordered_uuidv7_ids() {
        let t = temp_store().await;
        for kind in [RowKind::Create, RowKind::Update, RowKind::Delete] {
            apply(&t.store, DOC, move |_| vec![DocOp::InsertMarker(kind)]).await;
        }
        let rows = snapshot(&t.store, DOC).await.rows;
        assert_eq!(
            rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
            vec![RowKind::Create, RowKind::Update, RowKind::Delete]
        );
        assert!(rows.iter().all(|r| r.mutation_id.get_version_num() == 7));
        assert!(rows.windows(2).all(|w| w[0].mutation_id < w[1].mutation_id));
    }

    #[tokio::test]
    async fn parked_rows_load_as_parked() {
        let t = temp_store().await;
        apply(&t.store, DOC, |_| {
            vec![DocOp::InsertMarker(RowKind::Update)]
        })
        .await;
        apply(&t.store, DOC, |snap| {
            vec![DocOp::Park {
                rows: snap.rows.iter().map(|r| r.mutation_id).collect(),
                error: "validation".into(),
            }]
        })
        .await;
        assert!(snapshot(&t.store, DOC).await.rows[0].parked);
        let error: String = sqlx::query_scalar("SELECT parked_error FROM outbox")
            .fetch_one(&t.store.pool)
            .await
            .unwrap();
        assert_eq!(error, "validation");
    }

    #[tokio::test]
    async fn only_visible_changes_are_logged() {
        let t = temp_store().await;
        seed_synced(&t.store, DOC, SCOPE_OWN, Some(ME), json!({"a": 1}), 1).await;
        apply(&t.store, DOC, |snap| {
            vec![DocOp::SetShadow(Shadow {
                content: snap.content.clone(),
                hash: "server-hash".into(),
                seq: 2,
            })]
        })
        .await;
        apply(&t.store, DOC, |_| vec![DocOp::SetContent(json!({"a": 2}))]).await;
        apply(&t.store, DOC, |_| vec![DocOp::HardDelete]).await;

        let log: Vec<(String, String)> =
            sqlx::query_as("SELECT kind, origin FROM change_log ORDER BY local_seq")
                .fetch_all(&t.store.pool)
                .await
                .unwrap();
        let expected = [
            ("upsert", "server"),
            ("upsert", "server"),
            ("delete", "server"),
        ];
        assert_eq!(
            log,
            expected
                .iter()
                .map(|(k, o)| (k.to_string(), o.to_string()))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn undelete_clears_the_soft_delete_and_logs_an_upsert() {
        let t = temp_store().await;
        seed_synced(&t.store, DOC, SCOPE_OWN, Some(ME), json!({"a": 1}), 1).await;
        t.store.delete_document(DOC).await.unwrap();
        apply(&t.store, DOC, |_| vec![DocOp::DropAllRows, DocOp::Undelete]).await;
        let after = snapshot(&t.store, DOC).await;
        assert!(after.exists && !after.soft_deleted);
        assert!(after.rows.is_empty());
        let last_kind: String =
            sqlx::query_scalar("SELECT kind FROM change_log ORDER BY local_seq DESC LIMIT 1")
                .fetch_one(&t.store.pool)
                .await
                .unwrap();
        assert_eq!(last_kind, "upsert", "the host is told the document is back");
    }

    #[tokio::test]
    async fn title_hash_and_envelope_metadata_follow_the_applied_version() {
        let t = temp_store().await;
        let mut doc = envelope(DOC, None, json!({"title": "Just Intonation"}), 5);
        doc.author_id = Some(Uuid::from_u128(0xB));
        doc.source_doc_id = Some(Uuid::from_u128(0x5));
        let change = upsert_change(SCOPE_CURATED, doc.clone());

        apply_with_envelope(&t.store, DOC, Some(&doc), |snap| {
            apply_change(snap, &change, ME, &APPEND)
        })
        .await;

        let (title, hash, author, source): (String, String, String, String) = sqlx::query_as(
            "SELECT title, hash, author_id, source_doc_id FROM documents WHERE id = ?",
        )
        .bind(DOC.to_string())
        .fetch_one(&t.store.pool)
        .await
        .unwrap();
        assert_eq!(title, "Just Intonation");
        assert_eq!(hash, content_hash(&json!({"title": "Just Intonation"})));
        assert_eq!(author, Uuid::from_u128(0xB).to_string());
        assert_eq!(source, Uuid::from_u128(0x5).to_string());
    }

    #[tokio::test]
    async fn server_content_without_a_server_seq_is_corrupt() {
        let t = temp_store().await;
        exec(
            &t.store,
            &format!(
                "INSERT INTO documents (id, user_id, content, hash, server_content, server_hash, \
                 created_at, updated_at) VALUES ('{DOC}', '{ME}', '{{}}', 'h', '{{}}', 'h', 't', 't')"
            ),
        )
        .await;
        let mut tx = t.store.begin().await.unwrap();
        assert!(matches!(
            load_snapshot(&mut tx, DOC).await,
            Err(StoreError::Corrupt(_))
        ));
    }
}
