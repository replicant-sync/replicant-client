use serde_json::Value;
use uuid::Uuid;

use super::hash::content_hash;
use super::types::{Change, ChangeKind, DocEnvelope, Scope, Seq};

#[derive(Debug, Clone, PartialEq)]
pub struct Shadow {
    pub content: Value,
    pub hash: String,
    pub seq: Seq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Create,
    Update,
    Delete,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutboxRow {
    pub mutation_id: Uuid,
    pub kind: RowKind,
    pub parked: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Membership {
    pub scope: Scope,
    pub member: bool,
    pub seq: Seq,
}

/// Everything the per-document rules need, loaded by the driver in one transaction.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DocSnapshot {
    pub doc_id: Uuid,
    /// A local document row exists (soft-deleted rows still exist).
    pub exists: bool,
    pub content: Value,
    pub owner_id: Option<Uuid>,
    pub read_only: bool,
    pub soft_deleted: bool,
    pub shadow: Option<Shadow>,
    /// Sorted by `mutation_id`.
    pub rows: Vec<OutboxRow>,
    pub memberships: Vec<Membership>,
    pub tombstone_seq: Option<Seq>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverReason {
    DeleteWins,
    Conflict,
    BecamePublication,
    CreateRejected,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DocEvent {
    ConflictDetected,
    SyncError { code: String },
}

/// A write the driver performs inside the same transaction that loaded the snapshot.
#[derive(Debug, Clone, PartialEq)]
pub enum DocOp {
    SetShadow(Shadow),
    /// Sets local content; creates the local row if it does not exist.
    SetContent(Value),
    SetMeta {
        owner_id: Option<Uuid>,
        read_only: bool,
    },
    DeleteRows(Vec<Uuid>),
    DropAllRows,
    /// Driver generates a fresh UUIDv7 mutation id.
    InsertMarker(RowKind),
    Park {
        rows: Vec<Uuid>,
        error: String,
    },
    HardDelete,
    RecordTombstone(Seq),
    SetMembership(Membership),
    Recover {
        content: Value,
        reason: RecoverReason,
    },
    Emit(DocEvent),
}

const PROJECTED_MARKER: Uuid = Uuid::from_u128(u128::MAX);

impl DocSnapshot {
    pub fn server_seq(&self) -> Seq {
        let shadow_seq = self.shadow.as_ref().map_or(0, |s| s.seq);
        shadow_seq.max(self.tombstone_seq.unwrap_or(0))
    }

    pub fn writable(&self, me: Uuid) -> bool {
        self.owner_id == Some(me) && !self.read_only && !self.soft_deleted
    }

    /// The snapshot as it would look after the driver applied `ops`.
    pub fn project(&self, ops: &[DocOp]) -> DocSnapshot {
        let mut s = self.clone();
        for op in ops {
            match op {
                DocOp::SetShadow(sh) => s.shadow = Some(sh.clone()),
                DocOp::SetContent(v) => {
                    s.content = v.clone();
                    s.exists = true;
                }
                DocOp::SetMeta {
                    owner_id,
                    read_only,
                } => {
                    s.owner_id = *owner_id;
                    s.read_only = *read_only;
                }
                DocOp::DeleteRows(ids) => s.rows.retain(|r| !ids.contains(&r.mutation_id)),
                DocOp::DropAllRows => s.rows.clear(),
                DocOp::InsertMarker(kind) => s.rows.push(OutboxRow {
                    mutation_id: PROJECTED_MARKER,
                    kind: *kind,
                    parked: false,
                }),
                DocOp::Park { rows, .. } => {
                    for r in s.rows.iter_mut().filter(|r| rows.contains(&r.mutation_id)) {
                        r.parked = true;
                    }
                }
                DocOp::HardDelete => {
                    s.exists = false;
                    s.content = Value::Null;
                    s.shadow = None;
                    s.soft_deleted = false;
                }
                DocOp::RecordTombstone(seq) => s.tombstone_seq = Some(*seq),
                DocOp::SetMembership(mm) => {
                    match s.memberships.iter_mut().find(|x| x.scope == mm.scope) {
                        Some(existing) => *existing = mm.clone(),
                        None => s.memberships.push(mm.clone()),
                    }
                }
                DocOp::Recover { .. } | DocOp::Emit(_) => {}
            }
        }
        s
    }
}

/// Appends a marker when `ops` would leave a writable document with no rows but content
/// that differs from its shadow. Both sides are hashed locally: the server-supplied hash can
/// differ for identical content after jsonb normalisation.
pub fn with_settle_invariant(snap: &DocSnapshot, mut ops: Vec<DocOp>, me: Uuid) -> Vec<DocOp> {
    let after = snap.project(&ops);
    if !after.exists || !after.writable(me) || !after.rows.is_empty() {
        return ops;
    }
    match &after.shadow {
        None => ops.push(DocOp::InsertMarker(RowKind::Create)),
        Some(sh) if content_hash(&sh.content) != content_hash(&after.content) => {
            ops.push(DocOp::InsertMarker(RowKind::Update))
        }
        Some(_) => {}
    }
    ops
}

pub(crate) fn conflict_ops(snap: &DocSnapshot) -> Vec<DocOp> {
    let mut ops = vec![DocOp::Recover {
        content: snap.content.clone(),
        reason: RecoverReason::Conflict,
    }];
    if let Some(sh) = &snap.shadow {
        ops.push(DocOp::SetContent(sh.content.clone()));
    }
    ops.push(DocOp::DropAllRows);
    ops.push(DocOp::Emit(DocEvent::ConflictDetected));
    ops
}

fn membership_seq(snap: &DocSnapshot, scope: &str) -> Seq {
    snap.memberships
        .iter()
        .find(|m| m.scope == scope)
        .map_or(0, |m| m.seq)
}

/// Non-echo upsert: content guard already passed.
fn apply_upsert(snap: &DocSnapshot, doc: &DocEnvelope, seq: Seq) -> Vec<DocOp> {
    let new_shadow = Shadow {
        content: doc.content.clone(),
        hash: doc.hash.clone(),
        seq,
    };
    let mut ops = vec![DocOp::SetMeta {
        owner_id: doc.owner_id,
        read_only: doc.read_only,
    }];
    let pending = snap.exists && !snap.rows.is_empty();

    if pending && doc.read_only {
        ops.push(DocOp::Recover {
            content: snap.content.clone(),
            reason: RecoverReason::BecamePublication,
        });
        ops.push(DocOp::DropAllRows);
        ops.push(DocOp::SetShadow(new_shadow));
        ops.push(DocOp::SetContent(doc.content.clone()));
        ops.push(DocOp::Emit(DocEvent::SyncError {
            code: "became_publication".into(),
        }));
        return ops;
    }
    if !pending {
        ops.push(DocOp::SetShadow(new_shadow));
        ops.push(DocOp::SetContent(doc.content.clone()));
        return ops;
    }
    let content = match &snap.shadow {
        // Migration only: no known base, keep local content; pending = diff(new shadow, content).
        None => snap.content.clone(),
        Some(old) => match rebase(&old.content, &doc.content, &snap.content) {
            Rebased::Clean(v) => v,
            Rebased::Conflict => {
                let with_new_shadow = snap.project(&[DocOp::SetShadow(new_shadow.clone())]);
                ops.push(DocOp::SetShadow(new_shadow));
                ops.extend(conflict_ops(&with_new_shadow));
                return ops;
            }
        },
    };
    ops.push(DocOp::SetShadow(new_shadow));
    ops.push(DocOp::SetContent(content));
    ops
}

pub fn apply_change(snap: &DocSnapshot, change: &Change, me: Uuid) -> Vec<DocOp> {
    let mut ops = Vec::new();
    let membership_applies = change.seq > membership_seq(snap, &change.scope);
    if membership_applies {
        ops.push(DocOp::SetMembership(Membership {
            scope: change.scope.clone(),
            member: change.kind == ChangeKind::Upsert,
            seq: change.seq,
        }));
    }

    if change.kind == ChangeKind::Leave {
        if membership_applies {
            let after = snap.project(&ops);
            if after.exists && after.rows.is_empty() && !after.memberships.iter().any(|m| m.member)
            {
                ops.push(DocOp::HardDelete);
            }
        }
        return ops;
    }

    if change.seq <= snap.server_seq() {
        return ops;
    }

    match change.kind {
        ChangeKind::Upsert => {
            if let Some(doc) = &change.doc {
                ops.extend(apply_upsert(snap, doc, change.seq));
            }
        }
        ChangeKind::Delete => ops.extend(apply_delete(snap, change, me)),
        ChangeKind::Leave => unreachable!(),
    }
    ops
}

/// Placeholder until Task 6; deletes are covered there.
fn apply_delete(_snap: &DocSnapshot, _change: &Change, _me: Uuid) -> Vec<DocOp> {
    Vec::new()
}

pub fn apply_server_copy(snap: &DocSnapshot, doc: &DocEnvelope, _me: Uuid) -> Vec<DocOp> {
    if doc.seq <= snap.server_seq() {
        return Vec::new();
    }
    apply_upsert(snap, doc, doc.seq)
}

/// `get_document` said the document does not exist on the server: pending rows become a create.
pub fn apply_server_missing(snap: &DocSnapshot) -> Vec<DocOp> {
    if snap.rows.is_empty() {
        return Vec::new();
    }
    vec![DocOp::DropAllRows, DocOp::InsertMarker(RowKind::Create)]
}

#[derive(Debug, Clone, PartialEq)]
pub enum Rebased {
    Clean(Value),
    Conflict,
}

/// Replays the local change (`old_base` → `local`) onto `new_base`.
pub fn rebase(old_base: &Value, new_base: &Value, local: &Value) -> Rebased {
    let local_patch = json_patch::diff(old_base, local);
    let mut out = new_base.clone();
    match json_patch::patch(&mut out, &local_patch) {
        Ok(()) => Rebased::Clean(out),
        Err(_) => Rebased::Conflict,
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use serde_json::json;

    pub const ME: Uuid = Uuid::from_u128(0xA);
    pub const OTHER: Uuid = Uuid::from_u128(0xB);
    pub const DOC: Uuid = Uuid::from_u128(0xD0C);

    pub fn m(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    pub fn shadow(content: Value, seq: Seq) -> Shadow {
        Shadow {
            hash: content_hash(&content),
            content,
            seq,
        }
    }

    pub fn row(n: u128, kind: RowKind) -> OutboxRow {
        OutboxRow {
            mutation_id: m(n),
            kind,
            parked: false,
        }
    }

    /// A synced, writable document owned by ME with no pending rows.
    pub fn synced(content: Value, seq: Seq) -> DocSnapshot {
        DocSnapshot {
            doc_id: DOC,
            exists: true,
            content: content.clone(),
            owner_id: Some(ME),
            read_only: false,
            soft_deleted: false,
            shadow: Some(shadow(content, seq)),
            rows: vec![],
            memberships: vec![Membership {
                scope: "own".into(),
                member: true,
                seq,
            }],
            tombstone_seq: None,
        }
    }

    pub fn empty() -> DocSnapshot {
        DocSnapshot {
            doc_id: DOC,
            ..Default::default()
        }
    }

    pub fn sample() -> Value {
        json!({"title": "A", "pitches": ["1/1"]})
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::fixtures::*;
    use super::*;
    use serde_json::json;

    #[test]
    fn server_seq_is_max_of_shadow_and_tombstone() {
        let mut s = synced(sample(), 5);
        assert_eq!(s.server_seq(), 5);
        s.tombstone_seq = Some(9);
        assert_eq!(s.server_seq(), 9);
        assert_eq!(empty().server_seq(), 0);
    }

    #[test]
    fn writable_requires_owner_not_read_only_not_deleted() {
        let s = synced(sample(), 1);
        assert!(s.writable(ME));
        assert!(!s.writable(OTHER));
        assert!(!DocSnapshot {
            read_only: true,
            ..s.clone()
        }
        .writable(ME));
        assert!(!DocSnapshot {
            soft_deleted: true,
            ..s
        }
        .writable(ME));
    }

    #[test]
    fn project_applies_ops_in_order() {
        let s = synced(sample(), 1);
        let after = s.project(&[
            DocOp::SetContent(json!({"title": "B"})),
            DocOp::InsertMarker(RowKind::Update),
            DocOp::SetMembership(Membership {
                scope: "own".into(),
                member: false,
                seq: 4,
            }),
        ]);
        assert_eq!(after.content, json!({"title": "B"}));
        assert_eq!(after.rows.len(), 1);
        assert_eq!(after.memberships.len(), 1);
        assert!(!after.memberships[0].member);
        let gone = after.project(&[
            DocOp::DropAllRows,
            DocOp::HardDelete,
            DocOp::RecordTombstone(7),
        ]);
        assert!(!gone.exists);
        assert!(gone.rows.is_empty());
        assert_eq!(gone.shadow, None);
        assert_eq!(gone.tombstone_seq, Some(7));
    }

    #[test]
    fn invariant_inserts_marker_when_content_differs_and_no_rows_left() {
        let mut s = synced(sample(), 1);
        s.content = json!({"title": "edited"});
        s.rows = vec![row(1, RowKind::Update)];
        let ops = with_settle_invariant(&s, vec![DocOp::DeleteRows(vec![m(1)])], ME);
        assert_eq!(ops.last(), Some(&DocOp::InsertMarker(RowKind::Update)));
    }

    #[test]
    fn invariant_uses_local_hashes_not_server_hash() {
        // Server hash differs (jsonb normalisation) but content is identical.
        let mut s = synced(sample(), 1);
        s.shadow.as_mut().unwrap().hash = "server-normalised-hash".into();
        s.rows = vec![row(1, RowKind::Update)];
        let ops = with_settle_invariant(&s, vec![DocOp::DeleteRows(vec![m(1)])], ME);
        assert!(!ops.iter().any(|op| matches!(op, DocOp::InsertMarker(_))));
    }

    #[test]
    fn invariant_never_fires_for_read_only_or_soft_deleted() {
        let mut s = synced(sample(), 1);
        s.content = json!({"x": 1});
        for snap in [
            DocSnapshot {
                read_only: true,
                ..s.clone()
            },
            DocSnapshot {
                soft_deleted: true,
                ..s.clone()
            },
            DocSnapshot {
                owner_id: Some(OTHER),
                ..s.clone()
            },
        ] {
            let ops = with_settle_invariant(&snap, vec![], ME);
            assert!(ops.is_empty(), "{snap:?}");
        }
    }

    #[test]
    fn invariant_marker_is_create_when_never_acknowledged() {
        let s = DocSnapshot {
            shadow: None,
            ..synced(sample(), 0)
        };
        let ops = with_settle_invariant(&s, vec![], ME);
        assert_eq!(ops, vec![DocOp::InsertMarker(RowKind::Create)]);
    }
}

#[cfg(test)]
pub(crate) mod change_fixtures {
    use super::fixtures::*;
    use crate::engine::types::{Change, ChangeKind, DocEnvelope};
    use serde_json::Value;
    use uuid::Uuid;

    pub fn env(content: Value, seq: i64) -> DocEnvelope {
        DocEnvelope {
            doc_id: DOC,
            owner_id: Some(ME),
            author_id: None,
            read_only: false,
            source_doc_id: None,
            derived_from: None,
            title: None,
            hash: crate::engine::hash::content_hash(&content),
            content,
            seq,
        }
    }

    pub fn upsert(scope: &str, content: Value, seq: i64) -> Change {
        Change {
            scope: scope.into(),
            seq,
            prev_seq: seq - 1,
            doc_id: DOC,
            kind: ChangeKind::Upsert,
            doc: Some(env(content, seq)),
            client_id: None,
            upload_id: None,
        }
    }

    pub fn kind_change(scope: &str, kind: ChangeKind, seq: i64) -> Change {
        Change {
            doc: None,
            kind,
            ..upsert(scope, Value::Null, seq)
        }
    }

    pub fn with_upload(mut c: Change, upload_id: Uuid) -> Change {
        c.upload_id = Some(upload_id);
        c
    }
}

#[cfg(test)]
mod apply_change_tests {
    use super::change_fixtures::*;
    use super::fixtures::*;
    use super::*;
    use crate::engine::types::ChangeKind;
    use serde_json::json;

    #[test]
    fn stale_change_is_skipped_for_content() {
        let s = synced(sample(), 10);
        let ops = apply_change(&s, &upsert("own", json!({"old": true}), 8), ME);
        assert!(!ops
            .iter()
            .any(|o| matches!(o, DocOp::SetContent(_) | DocOp::SetShadow(_))));
    }

    #[test]
    fn upsert_without_rows_replaces_content_and_shadow() {
        let s = synced(sample(), 1);
        let new = json!({"title": "B"});
        let after = s.project(&apply_change(&s, &upsert("own", new.clone(), 2), ME));
        assert_eq!(after.content, new);
        assert_eq!(after.shadow.unwrap().seq, 2);
    }

    #[test]
    fn upsert_creates_missing_document() {
        let after = empty().project(&apply_change(
            &empty(),
            &upsert("collection:curated", sample(), 3),
            ME,
        ));
        assert!(after.exists);
        assert_eq!(after.content, sample());
        assert!(after
            .memberships
            .iter()
            .any(|m| m.scope == "collection:curated" && m.member));
    }

    #[test]
    fn upsert_with_pending_rows_rebases_local_edits() {
        let mut s = synced(json!({"a": 1}), 1);
        s.content = json!({"a": 1, "mine": true});
        s.rows = vec![row(5, RowKind::Update)];
        let after = s.project(&apply_change(
            &s,
            &upsert("own", json!({"a": 1, "theirs": true}), 2),
            ME,
        ));
        assert_eq!(after.content, json!({"a": 1, "mine": true, "theirs": true}));
        assert_eq!(
            after.shadow.unwrap().content,
            json!({"a": 1, "theirs": true})
        );
        assert_eq!(after.rows.len(), 1);
    }

    #[test]
    fn rebase_conflict_recovers_local_and_takes_server() {
        let mut s = synced(json!({"a": {"x": 1}}), 1);
        s.content = json!({"a": {"x": 2}});
        s.rows = vec![row(5, RowKind::Update)];
        let ops = apply_change(&s, &upsert("own", json!({}), 2), ME);
        assert!(ops.contains(&DocOp::Recover {
            content: json!({"a": {"x": 2}}),
            reason: RecoverReason::Conflict
        }));
        assert!(ops.contains(&DocOp::Emit(DocEvent::ConflictDetected)));
        let after = s.project(&ops);
        assert_eq!(after.content, json!({}));
        assert!(after.rows.is_empty());
    }

    #[test]
    fn null_shadow_keeps_local_content() {
        let mut s = synced(json!({"mine": 1}), 0);
        s.shadow = None;
        s.rows = vec![row(5, RowKind::Update)];
        let after = s.project(&apply_change(
            &s,
            &upsert("own", json!({"server": 1}), 4),
            ME,
        ));
        assert_eq!(after.content, json!({"mine": 1}));
        assert_eq!(after.shadow.unwrap().content, json!({"server": 1}));
    }

    #[test]
    fn membership_guard_is_independent_of_content_guard() {
        // Doc already at server_seq 20; lagging scope Y delivers upsert@15.
        let s = synced(sample(), 20);
        let ops = apply_change(&s, &upsert("collection:y", sample(), 15), ME);
        assert!(ops.contains(&DocOp::SetMembership(Membership {
            scope: "collection:y".into(),
            member: true,
            seq: 15
        })));
        assert!(!ops.iter().any(|o| matches!(o, DocOp::SetContent(_))));
    }

    #[test]
    fn older_upsert_cannot_undo_newer_leave_in_same_scope() {
        let mut s = synced(sample(), 10);
        s.memberships.push(Membership {
            scope: "collection:y".into(),
            member: false,
            seq: 20,
        });
        let ops = apply_change(&s, &upsert("collection:y", sample(), 15), ME);
        assert!(!ops.iter().any(|o| matches!(o, DocOp::SetMembership(_))));
    }

    #[test]
    fn leave_removes_membership_and_deletes_when_member_nowhere() {
        let mut s = synced(sample(), 3);
        s.memberships = vec![Membership {
            scope: "collection:curated".into(),
            member: true,
            seq: 3,
        }];
        let after = s.project(&apply_change(
            &s,
            &kind_change("collection:curated", ChangeKind::Leave, 4),
            ME,
        ));
        assert!(!after.exists);
    }

    #[test]
    fn leave_keeps_document_still_member_elsewhere() {
        let mut s = synced(sample(), 3);
        s.memberships.push(Membership {
            scope: "collection:x".into(),
            member: true,
            seq: 3,
        });
        let after = s.project(&apply_change(
            &s,
            &kind_change("collection:x", ChangeKind::Leave, 4),
            ME,
        ));
        assert!(after.exists);
    }

    #[test]
    fn upsert_marking_read_only_with_pending_rows_recovers_edits() {
        let mut s = synced(json!({"a": 1}), 1);
        s.content = json!({"a": 2});
        s.rows = vec![row(5, RowKind::Update)];
        let mut c = upsert("collection:curated", json!({"a": 1}), 2);
        c.doc.as_mut().unwrap().read_only = true;
        let ops = apply_change(&s, &c, ME);
        assert!(ops.contains(&DocOp::Recover {
            content: json!({"a": 2}),
            reason: RecoverReason::BecamePublication
        }));
        assert!(ops.contains(&DocOp::Emit(DocEvent::SyncError {
            code: "became_publication".into()
        })));
        assert!(s.project(&ops).rows.is_empty());
    }

    #[test]
    fn server_copy_applies_with_content_guard() {
        let s = synced(sample(), 5);
        assert!(apply_server_copy(&s, &env(json!({"x": 1}), 4), ME).is_empty());
        let after = s.project(&apply_server_copy(&s, &env(json!({"x": 1}), 6), ME));
        assert_eq!(after.content, json!({"x": 1}));
    }

    #[test]
    fn server_missing_turns_pending_rows_into_a_create() {
        let mut s = synced(json!({"mine": 1}), 0);
        s.shadow = None;
        s.rows = vec![row(5, RowKind::Update)];
        let after = s.project(&apply_server_missing(&s));
        assert_eq!(after.rows.len(), 1);
        assert_eq!(after.rows[0].kind, RowKind::Create);
    }
}

#[cfg(test)]
mod rebase_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn independent_edits_merge() {
        let r = rebase(
            &json!({"a": 1}),
            &json!({"a": 1, "b": 2}),
            &json!({"a": 1, "c": 3}),
        );
        assert_eq!(r, Rebased::Clean(json!({"a": 1, "b": 2, "c": 3})));
    }

    #[test]
    fn no_local_change_takes_new_base() {
        let r = rebase(&json!({"a": 1}), &json!({"a": 2}), &json!({"a": 1}));
        assert_eq!(r, Rebased::Clean(json!({"a": 2})));
    }

    #[test]
    fn edit_under_removed_parent_conflicts() {
        let r = rebase(&json!({"a": {"x": 1}}), &json!({}), &json!({"a": {"x": 2}}));
        assert_eq!(r, Rebased::Conflict);
    }
}
