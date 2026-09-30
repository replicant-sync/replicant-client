use serde_json::Value;
use uuid::Uuid;

use super::doc::{
    conflict_ops, delete_pending, with_settle_invariant, DocEvent, DocOp, DocSnapshot,
    RecoverReason, RowKind, Shadow,
};
use super::hash::content_hash;
use super::types::{DocEnvelope, Seq, ServerError, Upload, UploadKind};

pub const MAX_MISMATCH_ATTEMPTS: u32 = 3;

/// Consecutive replies whose content differs from what was sent before the document is parked.
pub const MAX_DIVERGENT_REPLIES: u32 = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct InFlight {
    pub upload_id: Uuid,
    pub covered: Vec<Uuid>,
    pub kind: UploadKind,
    pub base_hash: Option<String>,
    /// In memory only; compared with the reply's content to log a divergence.
    pub sent_content: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BuildResult {
    Send { upload: Upload, inflight: InFlight },
    NeedsServerCopy,
    Nothing,
    SettleLocally(Vec<DocOp>),
}

pub fn build_upload(snap: &DocSnapshot, me: Uuid) -> BuildResult {
    let ends_in_delete = snap.rows.last().is_some_and(|r| r.kind == RowKind::Delete);
    // A pending delete goes out even behind parked rows: nothing else can unpark a deleted document.
    if snap.rows.is_empty() || (!ends_in_delete && snap.rows.iter().any(|r| r.parked)) {
        return BuildResult::Nothing;
    }
    let covered: Vec<Uuid> = snap.rows.iter().map(|r| r.mutation_id).collect();
    let upload_id = *covered.last().expect("rows not empty");
    let has_create = snap.rows.iter().any(|r| r.kind == RowKind::Create);

    let send = |kind: UploadKind,
                base_hash: Option<String>,
                payload: Value,
                sent_content: Value| BuildResult::Send {
        upload: Upload {
            upload_id,
            doc_id: snap.doc_id,
            kind,
            base_hash: base_hash.clone(),
            payload,
        },
        inflight: InFlight {
            upload_id,
            covered: covered.clone(),
            kind,
            base_hash,
            sent_content,
        },
    };

    match (&snap.shadow, ends_in_delete) {
        (shadow, true) => send(
            UploadKind::Delete,
            shadow.as_ref().map(|sh| sh.hash.clone()),
            Value::Null,
            Value::Null,
        ),
        (None, false) if has_create => send(
            UploadKind::Create,
            None,
            snap.content.clone(),
            snap.content.clone(),
        ),
        (None, false) => BuildResult::NeedsServerCopy,
        (Some(sh), false) => {
            let patch = json_patch::diff(&sh.content, &snap.content);
            if patch.0.is_empty() {
                return BuildResult::SettleLocally(with_settle_invariant(
                    snap,
                    vec![DocOp::DeleteRows(covered.clone())],
                    me,
                ));
            }
            let payload = serde_json::to_value(&patch).expect("patch serializes");
            send(
                UploadKind::Update,
                Some(sh.hash.clone()),
                payload,
                snap.content.clone(),
            )
        }
    }
}

/// A successful update or create whose reply holds content other than what was sent. A push
/// may have applied it already, so the reply need not advance the shadow.
pub fn reply_diverged(
    snap: &DocSnapshot,
    inflight: &InFlight,
    reply: &Result<DocEnvelope, ServerError>,
) -> bool {
    matches!(reply, Ok(doc) if inflight.kind != UploadKind::Delete
        && snap.exists
        && content_hash(&doc.content) != content_hash(&inflight.sent_content))
}

#[derive(Debug, Clone, PartialEq)]
pub enum SettleResult {
    Ops(Vec<DocOp>),
    FetchServerCopy,
    Retry {
        after_ms: Option<u64>,
        mismatch: bool,
    },
}

pub fn settle(
    snap: &DocSnapshot,
    inflight: &InFlight,
    reply: &Result<DocEnvelope, ServerError>,
    me: Uuid,
    mismatch_attempts: u32,
    divergent_replies: u32,
) -> SettleResult {
    if !snap.exists && inflight.kind != UploadKind::Delete {
        return SettleResult::Ops(vec![DocOp::DeleteRows(inflight.covered.clone())]);
    }
    let delete_done = |seq: Seq| {
        SettleResult::Ops(vec![
            DocOp::DropAllRows,
            DocOp::HardDelete,
            DocOp::RecordTombstone(seq),
        ])
    };
    match reply {
        Ok(doc) if inflight.kind == UploadKind::Delete => delete_done(doc.seq),
        Ok(doc) => {
            let advances = doc.seq > snap.server_seq();
            let diverged = reply_diverged(snap, inflight, reply);
            let park =
                diverged && divergent_replies >= MAX_DIVERGENT_REPLIES && !delete_pending(snap);
            let mut ops = if park {
                // A push of this upload may have replaced the covered rows with a marker.
                vec![DocOp::Park {
                    rows: snap.rows.iter().map(|r| r.mutation_id).collect(),
                    error: "diverged".into(),
                }]
            } else {
                vec![DocOp::DeleteRows(inflight.covered.clone())]
            };
            if advances {
                ops.push(DocOp::SetMeta {
                    owner_id: doc.owner_id,
                    read_only: doc.read_only,
                });
                // The server's copy, never our prediction of it: if they differ, the settle
                // invariant queues an upload of the difference.
                ops.push(DocOp::SetShadow(Shadow {
                    content: doc.content.clone(),
                    hash: doc.hash.clone(),
                    seq: doc.seq,
                }));
            }
            if park {
                ops.push(DocOp::Emit(DocEvent::SyncError {
                    code: "diverged".into(),
                }));
            }
            SettleResult::Ops(with_settle_invariant(snap, ops, me))
        }
        Err(e) => match e.code.as_str() {
            // The server copy decides: it supersedes the delete, or the delete goes out on it.
            "hash_mismatch" if inflight.kind == UploadKind::Delete => {
                if mismatch_attempts >= MAX_MISMATCH_ATTEMPTS {
                    SettleResult::Retry {
                        after_ms: None,
                        mismatch: false,
                    }
                } else {
                    SettleResult::FetchServerCopy
                }
            }
            "hash_mismatch" if mismatch_attempts >= MAX_MISMATCH_ATTEMPTS => {
                if delete_pending(snap) {
                    SettleResult::FetchServerCopy
                } else {
                    SettleResult::Ops(conflict_ops(snap))
                }
            }
            "hash_mismatch" => {
                let shadow_current =
                    snap.shadow.as_ref().map(|s| &s.hash) == e.current_hash.as_ref();
                if shadow_current {
                    SettleResult::Retry {
                        after_ms: None,
                        mismatch: true,
                    }
                } else {
                    SettleResult::FetchServerCopy
                }
            }
            "exists" if e.existing_owner == Some(me) => SettleResult::FetchServerCopy,
            "exists" => SettleResult::Ops(vec![
                DocOp::Recover {
                    content: snap.content.clone(),
                    reason: RecoverReason::CreateRejected,
                },
                DocOp::DropAllRows,
                DocOp::HardDelete,
                DocOp::Emit(DocEvent::SyncError {
                    code: "create_rejected".into(),
                }),
            ]),
            "not_found" if inflight.kind == UploadKind::Delete => delete_done(snap.server_seq()),
            "not_found" => SettleResult::Ops(vec![
                DocOp::Recover {
                    content: snap.content.clone(),
                    reason: RecoverReason::DeleteWins,
                },
                DocOp::Emit(DocEvent::ConflictDetected),
                DocOp::DropAllRows,
                DocOp::HardDelete,
            ]),
            "validation" | "forbidden" | "too_large" => SettleResult::Ops(vec![
                DocOp::Park {
                    rows: inflight.covered.clone(),
                    error: e.code.clone(),
                },
                DocOp::Emit(DocEvent::SyncError {
                    code: e.code.clone(),
                }),
            ]),
            _ => SettleResult::Retry {
                after_ms: e.retry_after_ms,
                mismatch: false,
            },
        },
    }
}

#[cfg(test)]
mod upload_tests {
    use super::super::doc::change_fixtures::*;
    use super::super::doc::fixtures::*;
    use super::super::doc::*;
    use super::*;
    use crate::engine::types::{ServerError, UploadKind};
    use serde_json::json;

    fn sent(b: BuildResult) -> (Upload, InFlight) {
        match b {
            BuildResult::Send { upload, inflight } => (upload, inflight),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn create_rows_without_shadow_send_full_content() {
        let s = DocSnapshot {
            shadow: None,
            rows: vec![row(1, RowKind::Create), row(2, RowKind::Update)],
            ..synced(sample(), 0)
        };
        let (u, f) = sent(build_upload(&s, ME));
        assert_eq!(u.kind, UploadKind::Create);
        assert_eq!(u.payload, sample());
        assert_eq!(u.upload_id, m(2));
        assert_eq!(f.covered, vec![m(1), m(2)]);
    }

    #[test]
    fn create_then_delete_sends_unconditional_delete() {
        let s = DocSnapshot {
            shadow: None,
            soft_deleted: true,
            rows: vec![row(1, RowKind::Create), row(2, RowKind::Delete)],
            ..synced(sample(), 0)
        };
        let (u, f) = sent(build_upload(&s, ME));
        assert_eq!(u.kind, UploadKind::Delete);
        assert_eq!(u.base_hash, None);
        assert_eq!(f.covered, vec![m(1), m(2)]);
    }

    #[test]
    fn migrated_update_then_delete_sends_delete() {
        let s = DocSnapshot {
            shadow: None,
            soft_deleted: true,
            rows: vec![row(1, RowKind::Update), row(2, RowKind::Delete)],
            ..synced(sample(), 0)
        };
        let (u, _) = sent(build_upload(&s, ME));
        assert_eq!(u.kind, UploadKind::Delete);
    }

    #[test]
    fn updates_send_diff_against_shadow_with_base_hash() {
        let mut s = synced(json!({"a": 1}), 4);
        s.content = json!({"a": 2});
        s.rows = vec![row(7, RowKind::Update)];
        let (u, f) = sent(build_upload(&s, ME));
        assert_eq!(u.kind, UploadKind::Update);
        assert_eq!(
            u.base_hash,
            Some(crate::engine::hash::content_hash(&json!({"a": 1})))
        );
        assert_eq!(
            u.payload,
            json!([{"op": "replace", "path": "/a", "value": 2}])
        );
        assert_eq!(f.sent_content, json!({"a": 2}));
    }

    #[test]
    fn delete_with_shadow_carries_the_shadow_hash() {
        let mut s = synced(sample(), 4);
        s.soft_deleted = true;
        s.rows = vec![row(1, RowKind::Update), row(2, RowKind::Delete)];
        let (u, f) = sent(build_upload(&s, ME));
        assert_eq!(u.kind, UploadKind::Delete);
        assert_eq!(
            u.base_hash,
            Some(crate::engine::hash::content_hash(&sample()))
        );
        assert_eq!(f.base_hash, u.base_hash);
    }

    #[test]
    fn updates_with_null_shadow_need_server_copy() {
        let s = DocSnapshot {
            shadow: None,
            rows: vec![row(1, RowKind::Update)],
            ..synced(sample(), 0)
        };
        assert_eq!(build_upload(&s, ME), BuildResult::NeedsServerCopy);
    }

    #[test]
    fn build_after_echo_is_empty_diff_settle() {
        let mut s = synced(json!({"a": 2}), 5);
        s.rows = vec![row(1, RowKind::Update)];
        match build_upload(&s, ME) {
            BuildResult::SettleLocally(ops) => {
                let after = s.project(&ops);
                assert!(after.rows.is_empty());
                assert!(!ops.iter().any(|o| matches!(o, DocOp::InsertMarker(_))));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parked_rows_are_not_uploaded() {
        let mut s = synced(json!({"a": 1}), 1);
        s.content = json!({"a": 2});
        s.rows = vec![OutboxRow {
            parked: true,
            ..row(1, RowKind::Update)
        }];
        assert_eq!(build_upload(&s, ME), BuildResult::Nothing);
    }

    fn inflight_update(covered: Vec<Uuid>, sent_content: Value) -> InFlight {
        InFlight {
            upload_id: *covered.last().unwrap(),
            covered,
            kind: UploadKind::Update,
            base_hash: Some("h".into()),
            sent_content,
        }
    }

    #[test]
    fn success_deletes_covered_rows_only() {
        let mut s = synced(json!({"a": 1}), 1);
        s.content = json!({"a": 3});
        s.rows = vec![row(1, RowKind::Update), row(2, RowKind::Update)];
        let f = inflight_update(vec![m(1)], json!({"a": 2}));
        let SettleResult::Ops(ops) = settle(&s, &f, &Ok(env(json!({"a": 2}), 2)), ME, 0, 0) else {
            panic!()
        };
        let after = s.project(&ops);
        assert_eq!(
            after.rows.iter().map(|r| r.mutation_id).collect::<Vec<_>>(),
            vec![m(2)]
        );
        assert_eq!(after.content, json!({"a": 3}));
    }

    #[test]
    fn success_shadow_takes_the_replys_content() {
        let mut s = synced(json!({"n": 1}), 1);
        s.content = json!({"n": 100});
        s.rows = vec![row(1, RowKind::Update)];
        let f = inflight_update(vec![m(1)], json!({"n": 100}));
        let mut reply = env(json!({"n": 100}), 2);
        reply.hash = "server-hash".into();
        let SettleResult::Ops(ops) = settle(&s, &f, &Ok(reply), ME, 0, 0) else {
            panic!()
        };
        let after = s.project(&ops);
        assert_eq!(after.shadow.as_ref().unwrap().content, json!({"n": 100}));
        assert_eq!(
            after.shadow.unwrap().hash,
            "server-hash",
            "the server's hash, verbatim"
        );
        assert!(!ops.iter().any(|o| matches!(o, DocOp::InsertMarker(_))));
    }

    #[test]
    fn a_reply_that_differs_from_what_was_sent_queues_a_corrective_upload() {
        let mut s = synced(json!({"n": 1}), 1);
        s.content = json!({"n": 2});
        s.rows = vec![row(1, RowKind::Update)];
        let f = inflight_update(vec![m(1)], json!({"n": 2}));
        let reply = env(json!({"n": 2, "added": true}), 2);
        let SettleResult::Ops(ops) = settle(&s, &f, &Ok(reply), ME, 0, 0) else {
            panic!()
        };
        let after = s.project(&ops);
        assert_eq!(
            after.shadow.unwrap().content,
            json!({"n": 2, "added": true})
        );
        assert_eq!(
            after.content,
            json!({"n": 2}),
            "a reply never overwrites local content"
        );
        assert_eq!(after.rows.len(), 1, "a marker uploads the difference");
    }

    fn divergent_settle(divergent_replies: u32) -> (DocSnapshot, Vec<DocOp>) {
        let mut s = synced(json!({"n": 1}), 1);
        s.content = json!({"n": 2});
        s.rows = vec![row(1, RowKind::Update)];
        let f = inflight_update(vec![m(1)], json!({"n": 2}));
        let reply = env(json!({"n": 2, "added": true}), 2);
        let SettleResult::Ops(ops) = settle(&s, &f, &Ok(reply), ME, 0, divergent_replies) else {
            panic!()
        };
        (s, ops)
    }

    #[test]
    fn a_divergent_reply_below_the_limit_queues_a_marker_without_parking() {
        let (s, ops) = divergent_settle(MAX_DIVERGENT_REPLIES - 1);
        assert!(!ops.iter().any(|o| matches!(o, DocOp::Park { .. })));
        assert!(!ops.iter().any(|o| matches!(o, DocOp::Emit(_))));
        assert_eq!(s.project(&ops).rows.len(), 1);
    }

    #[test]
    fn a_divergent_reply_at_the_limit_parks_the_rows_and_keeps_local_content() {
        let (s, ops) = divergent_settle(MAX_DIVERGENT_REPLIES);
        assert!(ops.contains(&DocOp::Park {
            rows: vec![m(1)],
            error: "diverged".into()
        }));
        assert!(ops.contains(&DocOp::Emit(DocEvent::SyncError {
            code: "diverged".into()
        })));
        let after = s.project(&ops);
        assert_eq!(after.content, json!({"n": 2}));
        assert_eq!(
            after.shadow.unwrap().content,
            json!({"n": 2, "added": true}),
            "the reply's copy is still adopted"
        );
        assert!(after.rows.iter().all(|r| r.parked));
    }

    #[test]
    fn a_pending_delete_is_built_behind_parked_rows() {
        let mut s = synced(json!({"n": 1}), 1);
        s.soft_deleted = true;
        let mut parked = row(1, RowKind::Update);
        parked.parked = true;
        s.rows = vec![parked, row(2, RowKind::Delete)];
        let (upload, inflight) = sent(build_upload(&s, ME));
        assert_eq!(upload.kind, UploadKind::Delete);
        assert_eq!(
            upload.base_hash,
            s.shadow.as_ref().map(|sh| sh.hash.clone())
        );
        assert_eq!(inflight.covered, vec![m(1), m(2)]);
    }

    #[test]
    fn a_delete_made_during_the_last_divergent_upload_is_not_parked() {
        let mut s = synced(json!({"n": 1}), 1);
        s.content = json!({"n": 2});
        s.soft_deleted = true;
        s.rows = vec![row(1, RowKind::Update), row(2, RowKind::Delete)];
        let f = inflight_update(vec![m(1)], json!({"n": 2}));
        let reply = env(json!({"n": 2, "added": true}), 2);
        let SettleResult::Ops(ops) = settle(&s, &f, &Ok(reply), ME, 0, MAX_DIVERGENT_REPLIES)
        else {
            panic!()
        };
        assert!(!ops.iter().any(|o| matches!(o, DocOp::Park { .. })));
        let after = s.project(&ops);
        assert!(after.rows.iter().all(|r| !r.parked));
        let (upload, _) = sent(build_upload(&after, ME));
        assert_eq!(upload.kind, UploadKind::Delete);
    }

    #[test]
    fn success_older_than_shadow_only_deletes_rows() {
        let mut s = synced(json!({"n": 5}), 9);
        s.rows = vec![row(1, RowKind::Update)];
        let f = inflight_update(vec![m(1)], json!({"n": 4}));
        let SettleResult::Ops(ops) = settle(&s, &f, &Ok(env(json!({"n": 4}), 7)), ME, 0, 0) else {
            panic!()
        };
        assert!(!ops.iter().any(|o| matches!(o, DocOp::SetShadow(_))));
    }

    #[test]
    fn delete_success_hard_deletes() {
        let mut s = synced(sample(), 1);
        s.soft_deleted = true;
        s.rows = vec![row(1, RowKind::Delete)];
        let f = InFlight {
            kind: UploadKind::Delete,
            base_hash: None,
            ..inflight_update(vec![m(1)], Value::Null)
        };
        let SettleResult::Ops(ops) = settle(&s, &f, &Ok(env(Value::Null, 2)), ME, 0, 0) else {
            panic!()
        };
        let after = s.project(&ops);
        assert!(!after.exists);
        assert_eq!(after.tombstone_seq, Some(2));
    }

    #[test]
    fn not_found_on_delete_upload_settles_locally() {
        let mut s = synced(sample(), 1);
        s.soft_deleted = true;
        s.rows = vec![row(1, RowKind::Delete)];
        let f = InFlight {
            kind: UploadKind::Delete,
            base_hash: None,
            ..inflight_update(vec![m(1)], Value::Null)
        };
        let SettleResult::Ops(ops) = settle(&s, &f, &Err(ServerError::new("not_found")), ME, 0, 0)
        else {
            panic!()
        };
        let after = s.project(&ops);
        assert!(!after.exists);
        assert!(after.rows.is_empty());
    }

    fn mismatch(current_hash: &str) -> ServerError {
        ServerError {
            current_hash: Some(current_hash.into()),
            current_seq: Some(9),
            ..ServerError::new("hash_mismatch")
        }
    }

    #[test]
    fn mismatch_with_stale_shadow_fetches_server_copy() {
        let s = DocSnapshot {
            rows: vec![row(1, RowKind::Update)],
            ..synced(json!({"a": 1}), 1)
        };
        let f = inflight_update(vec![m(1)], json!({"a": 2}));
        assert_eq!(
            settle(&s, &f, &Err(mismatch("other")), ME, 0, 0),
            SettleResult::FetchServerCopy
        );
    }

    #[test]
    fn mismatch_with_current_shadow_retries_immediately() {
        let s = DocSnapshot {
            rows: vec![row(1, RowKind::Update)],
            ..synced(json!({"a": 1}), 1)
        };
        let current = s.shadow.as_ref().unwrap().hash.clone();
        let f = inflight_update(vec![m(1)], json!({"a": 2}));
        assert_eq!(
            settle(&s, &f, &Err(mismatch(&current)), ME, 0, 0),
            SettleResult::Retry {
                after_ms: None,
                mismatch: true
            }
        );
    }

    #[test]
    fn third_mismatch_settles_as_conflict() {
        let mut s = synced(json!({"a": 1}), 1);
        s.content = json!({"a": 2});
        s.rows = vec![row(1, RowKind::Update)];
        let f = inflight_update(vec![m(1)], json!({"a": 2}));
        let SettleResult::Ops(ops) =
            settle(&s, &f, &Err(mismatch("x")), ME, MAX_MISMATCH_ATTEMPTS, 0)
        else {
            panic!()
        };
        assert!(ops.contains(&DocOp::Recover {
            content: json!({"a": 2}),
            reason: RecoverReason::Conflict
        }));
        assert_eq!(s.project(&ops).content, json!({"a": 1}));
    }

    #[test]
    fn mismatch_limit_with_a_pending_delete_fetches_the_server_copy() {
        let mut s = synced(json!({"a": 1}), 1);
        s.content = json!({"a": 2});
        s.soft_deleted = true;
        s.rows = vec![row(1, RowKind::Update), row(2, RowKind::Delete)];
        let f = inflight_update(vec![m(1)], json!({"a": 2}));
        assert_eq!(
            settle(&s, &f, &Err(mismatch("x")), ME, MAX_MISMATCH_ATTEMPTS, 0),
            SettleResult::FetchServerCopy
        );
    }

    fn inflight_delete(s: &DocSnapshot) -> InFlight {
        InFlight {
            kind: UploadKind::Delete,
            base_hash: s.shadow.as_ref().map(|sh| sh.hash.clone()),
            ..inflight_update(vec![m(1)], Value::Null)
        }
    }

    #[test]
    fn a_stale_delete_fetches_the_server_copy() {
        let mut s = synced(json!({"a": 1}), 1);
        s.soft_deleted = true;
        s.rows = vec![row(1, RowKind::Delete)];
        let f = inflight_delete(&s);
        assert_eq!(
            settle(&s, &f, &Err(mismatch("newer")), ME, 0, 0),
            SettleResult::FetchServerCopy
        );
        let current = s.shadow.as_ref().unwrap().hash.clone();
        assert_eq!(
            settle(&s, &f, &Err(mismatch(&current)), ME, 0, 0),
            SettleResult::FetchServerCopy,
            "a delete is never retried blind on the same base"
        );
    }

    #[test]
    fn a_delete_that_keeps_mismatching_backs_off() {
        let mut s = synced(json!({"a": 1}), 1);
        s.soft_deleted = true;
        s.rows = vec![row(1, RowKind::Delete)];
        assert_eq!(
            settle(
                &s,
                &inflight_delete(&s),
                &Err(mismatch("newer")),
                ME,
                MAX_MISMATCH_ATTEMPTS,
                0
            ),
            SettleResult::Retry {
                after_ms: None,
                mismatch: false
            }
        );
    }

    #[test]
    fn create_exists_and_mine_fetches_server_copy() {
        let s = DocSnapshot {
            shadow: None,
            rows: vec![row(1, RowKind::Create)],
            ..synced(sample(), 0)
        };
        let f = InFlight {
            kind: UploadKind::Create,
            base_hash: None,
            ..inflight_update(vec![m(1)], sample())
        };
        let err = ServerError {
            existing_owner: Some(ME),
            ..ServerError::new("exists")
        };
        assert_eq!(
            settle(&s, &f, &Err(err), ME, 0, 0),
            SettleResult::FetchServerCopy
        );
    }

    #[test]
    fn create_exists_not_mine_is_recovered() {
        let s = DocSnapshot {
            shadow: None,
            rows: vec![row(1, RowKind::Create)],
            ..synced(sample(), 0)
        };
        let f = InFlight {
            kind: UploadKind::Create,
            base_hash: None,
            ..inflight_update(vec![m(1)], sample())
        };
        let err = ServerError {
            existing_owner: Some(OTHER),
            ..ServerError::new("exists")
        };
        let SettleResult::Ops(ops) = settle(&s, &f, &Err(err), ME, 0, 0) else {
            panic!()
        };
        assert!(ops.contains(&DocOp::Recover {
            content: sample(),
            reason: RecoverReason::CreateRejected
        }));
        assert!(!s.project(&ops).exists);
    }

    #[test]
    fn fatal_item_error_parks_rows_and_reports_once() {
        let s = DocSnapshot {
            rows: vec![row(1, RowKind::Update)],
            ..synced(sample(), 1)
        };
        let f = inflight_update(vec![m(1)], sample());
        let SettleResult::Ops(ops) = settle(&s, &f, &Err(ServerError::new("validation")), ME, 0, 0)
        else {
            panic!()
        };
        assert_eq!(
            ops,
            vec![
                DocOp::Park {
                    rows: vec![m(1)],
                    error: "validation".into()
                },
                DocOp::Emit(DocEvent::SyncError {
                    code: "validation".into()
                }),
            ]
        );
    }

    #[test]
    fn not_found_on_update_is_delete_wins() {
        let mut s = synced(json!({"a": 1}), 1);
        s.content = json!({"a": 2});
        s.rows = vec![row(1, RowKind::Update)];
        let f = inflight_update(vec![m(1)], json!({"a": 2}));
        let SettleResult::Ops(ops) = settle(&s, &f, &Err(ServerError::new("not_found")), ME, 0, 0)
        else {
            panic!()
        };
        assert!(ops.contains(&DocOp::Recover {
            content: json!({"a": 2}),
            reason: RecoverReason::DeleteWins
        }));
        assert!(!s.project(&ops).exists);
    }

    #[test]
    fn transient_error_retries_with_server_delay() {
        let s = DocSnapshot {
            rows: vec![row(1, RowKind::Update)],
            ..synced(sample(), 1)
        };
        let f = inflight_update(vec![m(1)], sample());
        let err = ServerError {
            retry_after_ms: Some(5000),
            ..ServerError::new("internal")
        };
        assert_eq!(
            settle(&s, &f, &Err(err), ME, 0, 0),
            SettleResult::Retry {
                after_ms: Some(5000),
                mismatch: false
            }
        );
    }

    #[test]
    fn reply_for_vanished_document_is_noop() {
        let s = empty();
        let f = inflight_update(vec![m(1)], sample());
        assert_eq!(
            settle(&s, &f, &Ok(env(sample(), 3)), ME, 0, 0),
            SettleResult::Ops(vec![DocOp::DeleteRows(vec![m(1)])])
        );
    }
}
