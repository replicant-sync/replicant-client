use serde_json::Value;
use uuid::Uuid;

use super::hash::content_hash;
use super::types::{Change, ChangeKind, DocEnvelope, Scope, Seq};

pub use super::doc_upload::{
    build_upload, settle, BuildResult, InFlight, SettleResult, MAX_MISMATCH_ATTEMPTS,
};

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

pub(crate) fn delete_pending(snap: &DocSnapshot) -> bool {
    snap.soft_deleted || snap.rows.last().is_some_and(|r| r.kind == RowKind::Delete)
}

fn membership_seq(snap: &DocSnapshot, scope: &str) -> Seq {
    snap.memberships
        .iter()
        .find(|m| m.scope == scope)
        .map_or(0, |m| m.seq)
}

/// Non-echo upsert: content guard already passed. When `local_base_known` is false the shadow
/// is not the base of the pending rows, so edits that would need a rebase settle as a conflict.
fn apply_upsert(
    snap: &DocSnapshot,
    doc: &DocEnvelope,
    seq: Seq,
    local_base_known: bool,
) -> Vec<DocOp> {
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
    // The pending delete is unconditional and will win; leave content and rows untouched.
    if delete_pending(snap) {
        ops.push(DocOp::SetShadow(new_shadow));
        return ops;
    }
    let rebased = match &snap.shadow {
        _ if !local_base_known => None,
        // Migration only: no known base, keep local content; pending = diff(new shadow, content).
        None => Some(snap.content.clone()),
        Some(old) => match rebase(&old.content, &doc.content, &snap.content) {
            Rebased::Clean(v) => Some(v),
            Rebased::Conflict => None,
        },
    };
    let Some(content) = rebased else {
        let with_new_shadow = snap.project(&[DocOp::SetShadow(new_shadow.clone())]);
        ops.push(DocOp::SetShadow(new_shadow));
        ops.extend(conflict_ops(&with_new_shadow));
        return ops;
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

    let is_member = |applied: &[DocOp]| {
        snap.project(applied)
            .memberships
            .iter()
            .any(|m| m.scope == change.scope && m.member)
    };
    let echo_upload = change
        .upload_id
        .filter(|u| snap.rows.iter().any(|r| r.mutation_id == *u));

    match (change.kind, echo_upload) {
        // Echo of an upload from this data dir: never rebase, local content already holds the
        // uploaded delta and re-applying it would duplicate or conflict.
        (ChangeKind::Upsert, Some(upload_id)) => {
            if !is_member(&ops) {
                return ops;
            }
            if let Some(doc) = &change.doc {
                let covered: Vec<Uuid> = snap
                    .rows
                    .iter()
                    .filter(|r| r.mutation_id <= upload_id)
                    .map(|r| r.mutation_id)
                    .collect();
                // A newer envelope than the upload (a page without the stored reply): the upload
                // content is unknown, so rows it did not cover cannot be rebased onto it.
                if doc.seq != change.seq {
                    ops.push(DocOp::DeleteRows(covered));
                    let acked = snap.project(&ops);
                    ops.extend(apply_upsert(&acked, doc, doc.seq, false));
                    return with_settle_invariant(snap, ops, me);
                }
                ops.push(DocOp::SetMeta {
                    owner_id: doc.owner_id,
                    read_only: doc.read_only,
                });
                ops.push(DocOp::SetShadow(Shadow {
                    content: doc.content.clone(),
                    hash: doc.hash.clone(),
                    seq: change.seq,
                }));
                ops.push(DocOp::DeleteRows(covered));
                return with_settle_invariant(snap, ops, me);
            }
        }
        (ChangeKind::Upsert, None) => {
            debug_assert!(change.doc.is_some(), "upsert without doc");
            if is_member(&ops) {
                if let Some(doc) = &change.doc {
                    // Pages carry the current envelope; the shadow holds that version, not the change's.
                    ops.extend(apply_upsert(snap, doc, doc.seq.max(change.seq), true));
                }
            }
        }
        (ChangeKind::Delete, echo) => ops.extend(apply_delete(snap, change.seq, echo.is_some())),
        (ChangeKind::Leave, _) => unreachable!(),
    }
    ops
}

fn apply_delete(snap: &DocSnapshot, seq: Seq, is_echo: bool) -> Vec<DocOp> {
    let pending_delete = snap.rows.last().is_some_and(|r| r.kind == RowKind::Delete);
    let mut ops = Vec::new();
    if snap.exists && !snap.rows.is_empty() && !is_echo && !pending_delete {
        ops.push(DocOp::Recover {
            content: snap.content.clone(),
            reason: RecoverReason::DeleteWins,
        });
        ops.push(DocOp::Emit(DocEvent::ConflictDetected));
    }
    ops.push(DocOp::DropAllRows);
    if snap.exists {
        ops.push(DocOp::HardDelete);
    }
    ops.push(DocOp::RecordTombstone(seq));
    ops
}

/// `get_document` reported the document as deleted on the server (tombstoned).
pub fn apply_server_deleted(snap: &DocSnapshot, seq: Seq) -> Vec<DocOp> {
    apply_delete(snap, seq.max(snap.server_seq()), false)
}

pub fn apply_server_copy(snap: &DocSnapshot, doc: &DocEnvelope, _me: Uuid) -> Vec<DocOp> {
    if doc.seq <= snap.server_seq() {
        return Vec::new();
    }
    apply_upsert(snap, doc, doc.seq, true)
}

/// `get_document` said the document does not exist on the server: not_found means the server
/// never had it, so pending rows become a create — unless the pending row is itself a delete,
/// in which case the server's view and the local intent already agree.
pub fn apply_server_missing(snap: &DocSnapshot) -> Vec<DocOp> {
    match snap.rows.last() {
        None => Vec::new(),
        Some(r) if r.kind == RowKind::Delete => vec![DocOp::DropAllRows, DocOp::HardDelete],
        // build_upload resolves the marker into a create when there is no shadow, or into an
        // update when a shadow exists (the "exists and mine" flow).
        Some(_) => vec![DocOp::DropAllRows, DocOp::InsertMarker(RowKind::Create)],
    }
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
    fn page_upsert_stores_the_envelope_seq_so_older_page_changes_skip() {
        let s = synced(json!({"a": 1}), 1);
        // A page row for seq 2 carrying the document's current envelope (seq 5).
        let mut first = upsert("own", json!({}), 2);
        first.doc = Some(env(json!({"a": 3}), 5));
        let after = s.project(&apply_change(&s, &first, ME));
        assert_eq!(after.shadow.as_ref().unwrap().seq, 5);
        assert_eq!(after.content, json!({"a": 3}));

        let mut second = upsert("own", json!({}), 4);
        second.doc = Some(env(json!({"a": 3}), 5));
        let ops = apply_change(&after, &second, ME);
        assert!(!ops
            .iter()
            .any(|op| matches!(op, DocOp::SetShadow(_) | DocOp::SetContent(_))));
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
    fn upsert_conflict_on_soft_deleted_keeps_delete_pending() {
        let mut s = synced(json!({"a": {"x": 1}}), 1);
        s.content = json!({"a": {"x": 2}});
        s.soft_deleted = true;
        s.rows = vec![row(5, RowKind::Update), row(6, RowKind::Delete)];
        let ops = apply_change(&s, &upsert("own", json!({}), 2), ME);
        assert!(!ops.iter().any(|o| matches!(
            o,
            DocOp::Recover { .. } | DocOp::SetContent(_) | DocOp::Emit(_)
        )));
        let after = s.project(&ops);
        assert_eq!(after.rows, s.rows);
        assert_eq!(after.content, s.content);
        assert_eq!(after.shadow.unwrap().seq, 2);
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
    fn lagging_upsert_after_leave_does_not_recreate() {
        let mut s = synced(sample(), 3);
        s.memberships = vec![Membership {
            scope: "collection:curated".into(),
            member: true,
            seq: 3,
        }];
        let leave_ops = apply_change(
            &s,
            &kind_change("collection:curated", ChangeKind::Leave, 4),
            ME,
        );
        let after_leave = s.project(&leave_ops);
        let after = after_leave.project(&apply_change(
            &after_leave,
            &upsert("collection:curated", sample(), 3),
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

    #[test]
    fn server_missing_with_pending_delete_just_drops() {
        let mut s = synced(json!({"mine": 1}), 0);
        s.soft_deleted = true;
        s.shadow = None;
        s.rows = vec![row(5, RowKind::Update), row(6, RowKind::Delete)];
        let after = s.project(&apply_server_missing(&s));
        assert!(!after.exists);
        assert!(after.rows.is_empty());
    }
}

#[cfg(test)]
mod echo_and_delete_tests {
    use super::change_fixtures::*;
    use super::fixtures::*;
    use super::*;
    use crate::engine::types::ChangeKind;
    use serde_json::json;

    #[test]
    fn echo_settles_rows_and_keeps_content() {
        // Upload m1 sent {items:[x]}; user then appended y (row m2). Echo of m1 arrives.
        let mut s = synced(json!({"items": []}), 1);
        s.content = json!({"items": ["x", "y"]});
        s.rows = vec![row(1, RowKind::Update), row(2, RowKind::Update)];
        let echo = with_upload(upsert("own", json!({"items": ["x"]}), 2), m(1));
        let after = s.project(&apply_change(&s, &echo, ME));
        assert_eq!(
            after.content,
            json!({"items": ["x", "y"]}),
            "no double-apply"
        );
        assert_eq!(after.shadow.unwrap().content, json!({"items": ["x"]}));
        assert_eq!(
            after.rows.iter().map(|r| r.mutation_id).collect::<Vec<_>>(),
            vec![m(2)]
        );
    }

    /// The page's change for upload m1 (seq 2) carrying the current envelope (seq 4).
    fn page_echo(content: Value) -> Change {
        let mut c = with_upload(upsert("own", json!({}), 2), m(1));
        c.doc = Some(env(content, 4));
        c
    }

    /// Settled on the envelope at its own seq with no rows left.
    fn assert_adopted(after: &DocSnapshot, envelope: Value) {
        assert!(after.rows.is_empty());
        assert_eq!(after.content, envelope);
        let sh = after.shadow.as_ref().unwrap();
        assert_eq!(sh.content, envelope);
        assert_eq!(sh.seq, 4);
    }

    fn assert_no_conflict_or_marker(ops: &[DocOp]) {
        assert!(!ops.iter().any(|o| matches!(
            o,
            DocOp::Recover { .. } | DocOp::Emit(_) | DocOp::InsertMarker(_)
        )));
    }

    fn assert_conflict_recovers(ops: &[DocOp], local: Value) {
        assert!(ops.contains(&DocOp::Recover {
            content: local,
            reason: RecoverReason::Conflict
        }));
        assert!(ops.contains(&DocOp::Emit(DocEvent::ConflictDetected)));
        assert!(!ops.iter().any(|o| matches!(o, DocOp::InsertMarker(_))));
    }

    #[test]
    fn page_echo_with_newer_envelope_and_pending_rows_settles_as_conflict() {
        // m1 uploaded "mine"; m2 ("later") is still pending; the envelope also has "theirs".
        let mut s = synced(json!({"a": 1}), 1);
        s.content = json!({"a": 1, "mine": true, "later": true});
        s.rows = vec![row(1, RowKind::Update), row(2, RowKind::Update)];
        let envelope = json!({"a": 1, "mine": true, "theirs": true});

        let ops = apply_change(&s, &page_echo(envelope.clone()), ME);
        let after = s.project(&ops);

        assert_adopted(&after, envelope);
        assert_conflict_recovers(&ops, s.content.clone());
    }

    #[test]
    fn page_echo_array_append_covering_all_rows_adopts_envelope() {
        // m1 appended x; another device then appended z.
        let mut s = synced(json!({"items": []}), 1);
        s.content = json!({"items": ["x"]});
        s.rows = vec![row(1, RowKind::Update)];
        let envelope = json!({"items": ["x", "z"]});

        let ops = apply_change(&s, &page_echo(envelope.clone()), ME);

        assert_adopted(&s.project(&ops), envelope);
        assert_no_conflict_or_marker(&ops);
    }

    #[test]
    fn page_echo_array_append_with_pending_row_settles_as_conflict() {
        // m1 appended x (covered), m2 appended y (pending); another device appended z.
        let mut s = synced(json!({"items": []}), 1);
        s.content = json!({"items": ["x", "y"]});
        s.rows = vec![row(1, RowKind::Update), row(2, RowKind::Update)];
        let envelope = json!({"items": ["x", "z"]});

        let ops = apply_change(&s, &page_echo(envelope.clone()), ME);

        assert_adopted(&s.project(&ops), envelope);
        assert_conflict_recovers(&ops, json!({"items": ["x", "y"]}));
    }

    #[test]
    fn page_echo_key_removal_covering_all_rows_adopts_envelope() {
        // m1 removed k; another device then added b.
        let mut s = synced(json!({"a": 1, "k": 1}), 1);
        s.content = json!({"a": 1});
        s.rows = vec![row(1, RowKind::Update)];
        let envelope = json!({"a": 1, "b": 2});

        let ops = apply_change(&s, &page_echo(envelope.clone()), ME);

        assert_adopted(&s.project(&ops), envelope);
        assert_no_conflict_or_marker(&ops);
    }

    #[test]
    fn page_echo_key_removal_with_pending_row_settles_as_conflict() {
        // m1 removed k (covered), m2 added c (pending); another device added b.
        let mut s = synced(json!({"a": 1, "k": 1}), 1);
        s.content = json!({"a": 1, "c": 3});
        s.rows = vec![row(1, RowKind::Update), row(2, RowKind::Update)];
        let envelope = json!({"a": 1, "b": 2});

        let ops = apply_change(&s, &page_echo(envelope.clone()), ME);

        assert_adopted(&s.project(&ops), envelope);
        assert_conflict_recovers(&ops, json!({"a": 1, "c": 3}));
    }

    #[test]
    fn page_echo_covering_all_rows_takes_server_content() {
        let mut s = synced(json!({"n": 1}), 1);
        s.content = json!({"n": 2});
        s.rows = vec![row(1, RowKind::Update)];
        let mut page_echo = with_upload(upsert("own", json!({}), 2), m(1));
        page_echo.doc = Some(env(json!({"n": 2, "other": 1}), 5));

        let ops = apply_change(&s, &page_echo, ME);
        let after = s.project(&ops);

        assert!(after.rows.is_empty());
        assert_eq!(after.content, json!({"n": 2, "other": 1}));
        assert!(!ops.iter().any(|op| matches!(op, DocOp::InsertMarker(_))));
    }

    #[test]
    fn echo_covering_all_rows_settles_without_marker() {
        let mut s = synced(json!({"n": 1}), 1);
        s.content = json!({"n": 2});
        s.rows = vec![row(1, RowKind::Update)];
        let ops = apply_change(
            &s,
            &with_upload(upsert("own", json!({"n": 2}), 2), m(1)),
            ME,
        );
        let after = s.project(&ops);
        assert!(after.rows.is_empty());
        assert!(!ops.iter().any(|o| matches!(o, DocOp::InsertMarker(_))));
    }

    #[test]
    fn echo_of_create_sets_first_shadow() {
        let s = DocSnapshot {
            shadow: None,
            rows: vec![row(1, RowKind::Create)],
            ..synced(sample(), 0)
        };
        let after = s.project(&apply_change(
            &s,
            &with_upload(upsert("own", sample(), 3), m(1)),
            ME,
        ));
        assert_eq!(after.shadow.unwrap().seq, 3);
        assert!(after.rows.is_empty());
    }

    #[test]
    fn stale_echo_is_skipped_by_content_guard() {
        // Reply already settled rows and set shadow seq 2; echo arrives later.
        let s = synced(json!({"n": 2}), 2);
        let ops = apply_change(
            &s,
            &with_upload(upsert("own", json!({"n": 2}), 2), m(1)),
            ME,
        );
        assert!(!ops
            .iter()
            .any(|o| matches!(o, DocOp::SetContent(_) | DocOp::SetShadow(_))));
    }

    #[test]
    fn delete_echo_hard_deletes_and_records_tombstone() {
        let mut s = synced(sample(), 1);
        s.soft_deleted = true;
        s.rows = vec![row(1, RowKind::Delete)];
        let ops = apply_change(
            &s,
            &with_upload(kind_change("own", ChangeKind::Delete, 2), m(1)),
            ME,
        );
        let after = s.project(&ops);
        assert!(!after.exists);
        assert_eq!(after.tombstone_seq, Some(2));
        assert!(!ops
            .iter()
            .any(|o| matches!(o, DocOp::InsertMarker(_) | DocOp::Recover { .. })));
    }

    #[test]
    fn own_pending_delete_is_not_a_conflict() {
        let mut s = synced(sample(), 1);
        s.soft_deleted = true;
        s.rows = vec![row(1, RowKind::Update), row(2, RowKind::Delete)];
        let ops = apply_change(&s, &kind_change("own", ChangeKind::Delete, 2), ME);
        assert!(!ops
            .iter()
            .any(|o| matches!(o, DocOp::Recover { .. } | DocOp::Emit(_))));
        assert!(!s.project(&ops).exists);
    }

    #[test]
    fn delete_wins_over_pending_edits() {
        let mut s = synced(json!({"a": 1}), 1);
        s.content = json!({"a": 2});
        s.rows = vec![row(1, RowKind::Update)];
        let ops = apply_change(&s, &kind_change("own", ChangeKind::Delete, 2), ME);
        assert!(ops.contains(&DocOp::Recover {
            content: json!({"a": 2}),
            reason: RecoverReason::DeleteWins
        }));
        assert!(ops.contains(&DocOp::Emit(DocEvent::ConflictDetected)));
        let after = s.project(&ops);
        assert!(!after.exists);
        assert_eq!(after.tombstone_seq, Some(2));
    }

    #[test]
    fn delete_without_rows_deletes_and_tombstones() {
        let s = synced(sample(), 1);
        let after = s.project(&apply_change(
            &s,
            &kind_change("own", ChangeKind::Delete, 2),
            ME,
        ));
        assert!(!after.exists);
        assert_eq!(after.tombstone_seq, Some(2));
    }

    #[test]
    fn tombstone_blocks_recreation_by_lagging_scope() {
        let s = DocSnapshot {
            tombstone_seq: Some(20),
            ..empty()
        };
        let ops = apply_change(&s, &upsert("collection:y", sample(), 15), ME);
        assert!(!s.project(&ops).exists);
    }

    #[test]
    fn server_deleted_is_delete_wins_for_pending_edits() {
        let mut s = synced(json!({"a": 1}), 1);
        s.content = json!({"a": 2});
        s.rows = vec![row(1, RowKind::Update)];
        let ops = apply_server_deleted(&s, 9);
        assert!(ops.contains(&DocOp::Recover {
            content: json!({"a": 2}),
            reason: RecoverReason::DeleteWins
        }));
        let after = s.project(&ops);
        assert!(!after.exists);
        assert_eq!(after.tombstone_seq, Some(9));
    }

    #[test]
    fn server_deleted_never_moves_tombstone_backwards() {
        let s = DocSnapshot {
            tombstone_seq: Some(20),
            ..empty()
        };
        assert_eq!(
            s.project(&apply_server_deleted(&s, 5)).tombstone_seq,
            Some(20)
        );
        let s = synced(sample(), 7);
        assert_eq!(
            s.project(&apply_server_deleted(&s, 0)).tombstone_seq,
            Some(7)
        );
    }

    #[test]
    #[should_panic(expected = "upsert without doc")]
    fn upsert_without_doc_is_a_bug() {
        let s = synced(sample(), 1);
        apply_change(&s, &kind_change("own", ChangeKind::Upsert, 2), ME);
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

#[cfg(test)]
mod property_tests {
    //! One writable document against a model server that applies uploads (deduplicated by
    //! `(upload_id, base_hash)`). Changes reach the client as pushes (envelope at the change's
    //! seq) or catch-up pages (current envelope). A lost reply is a reconnect: every committed
    //! change is delivered before the next build.
    use std::collections::HashMap;

    use super::change_fixtures::env;
    use super::fixtures::*;
    use super::*;
    use crate::engine::backoff::Jitter;
    use crate::engine::types::{ServerError, Upload};
    use serde_json::json;

    const SEEDS: u64 = 500;
    const STEPS: usize = 80;
    const MAX_LOCAL_EDITS: u32 = 12;
    const MAX_OTHER_EDITS: u32 = 8;

    struct Server {
        /// Index = seq; seq 1 is the content both sides start from.
        history: Vec<Value>,
        changes: Vec<(Seq, Option<Uuid>)>,
        stored: HashMap<(Uuid, String), Seq>,
        other_edits: u32,
    }

    impl Server {
        fn new(content: Value) -> Server {
            Server {
                history: vec![Value::Null, content],
                changes: Vec::new(),
                stored: HashMap::new(),
                other_edits: 0,
            }
        }

        fn seq(&self) -> Seq {
            self.history.len() as Seq - 1
        }

        fn current(&self) -> &Value {
            &self.history[self.history.len() - 1]
        }

        fn envelope(&self, seq: Seq) -> DocEnvelope {
            env(self.history[seq as usize].clone(), seq)
        }

        fn commit(&mut self, content: Value, upload_id: Option<Uuid>) -> Seq {
            self.history.push(content);
            let seq = self.seq();
            self.changes.push((seq, upload_id));
            seq
        }

        fn other_device_edit(&mut self) {
            if self.other_edits == MAX_OTHER_EDITS {
                return;
            }
            self.other_edits += 1;
            let mut next = self.current().clone();
            next["theirs"] = json!(self.other_edits);
            self.commit(next, None);
        }

        fn upload(&mut self, upload: &Upload) -> Result<DocEnvelope, ServerError> {
            let base_hash = upload
                .base_hash
                .clone()
                .expect("the model only uploads updates");
            if let Some(seq) = self.stored.get(&(upload.upload_id, base_hash.clone())) {
                return Ok(self.envelope(*seq));
            }
            let current_hash = content_hash(self.current());
            if base_hash != current_hash {
                let mut error = ServerError::new("hash_mismatch");
                error.current_hash = Some(current_hash);
                error.current_seq = Some(self.seq());
                return Err(error);
            }
            let patch: json_patch::Patch = serde_json::from_value(upload.payload.clone()).unwrap();
            let mut next = self.current().clone();
            json_patch::patch(&mut next, &patch).unwrap();
            let seq = self.commit(next, Some(upload.upload_id));
            self.stored.insert((upload.upload_id, base_hash), seq);
            Ok(self.envelope(seq))
        }
    }

    struct Client {
        snap: DocSnapshot,
        next_row: u128,
        in_flight: Option<(InFlight, Result<DocEnvelope, ServerError>)>,
        mismatches: u32,
        delivered: usize,
        must_catch_up: bool,
        local_edits: u32,
        recovered: Vec<Value>,
    }

    impl Client {
        fn new(content: Value) -> Client {
            Client {
                snap: synced(content, 1),
                next_row: 0,
                in_flight: None,
                mismatches: 0,
                delivered: 0,
                must_catch_up: false,
                local_edits: 0,
                recovered: Vec::new(),
            }
        }

        fn push_row(&mut self, kind: RowKind) {
            self.next_row += 1;
            self.snap.rows.push(OutboxRow {
                mutation_id: m(self.next_row),
                kind,
                parked: false,
            });
        }

        /// Applies ops the way the store does: markers get fresh increasing ids.
        fn apply(&mut self, ops: &[DocOp]) {
            let seq_before = self.snap.server_seq();
            for op in ops {
                match op {
                    DocOp::InsertMarker(kind) => self.push_row(*kind),
                    DocOp::Recover { content, .. } => self.recovered.push(content.clone()),
                    other => self.snap = self.snap.project(std::slice::from_ref(other)),
                }
            }
            assert!(
                self.snap.server_seq() >= seq_before,
                "shadow seq went backwards"
            );
        }

        fn edit(&mut self) {
            if self.local_edits == MAX_LOCAL_EDITS {
                return;
            }
            self.local_edits += 1;
            let token = json!(format!("t{}", self.local_edits));
            self.snap.content["items"]
                .as_array_mut()
                .expect("items array")
                .push(token);
            self.push_row(RowKind::Update);
        }

        fn deliver(&mut self, server: &Server, as_page: bool) {
            let Some(&(seq, upload_id)) = server.changes.get(self.delivered) else {
                return;
            };
            self.delivered += 1;
            let doc = if as_page {
                server.envelope(server.seq())
            } else {
                server.envelope(seq)
            };
            let change = Change {
                scope: "own".into(),
                seq,
                prev_seq: seq - 1,
                doc_id: DOC,
                kind: ChangeKind::Upsert,
                doc: Some(doc),
                client_id: None,
                upload_id,
            };
            let ops = apply_change(&self.snap, &change, ME);
            self.apply(&ops);
        }

        fn catch_up(&mut self, server: &Server, rng: &mut Jitter) {
            while self.delivered < server.changes.len() {
                let as_page = rng.next_unit() < 0.5;
                self.deliver(server, as_page);
            }
            self.must_catch_up = false;
        }

        fn build(&mut self, server: &mut Server, rng: &mut Jitter) {
            if self.in_flight.is_some() {
                return;
            }
            if self.must_catch_up {
                self.catch_up(server, rng);
            }
            match build_upload(&self.snap, ME) {
                BuildResult::Send { upload, inflight } => {
                    let reply = server.upload(&upload);
                    self.in_flight = Some((inflight, reply));
                }
                BuildResult::SettleLocally(ops) => self.apply(&ops),
                BuildResult::Nothing => {}
                BuildResult::NeedsServerCopy => panic!("the model document always has a shadow"),
            }
        }

        fn reply(&mut self, server: &Server) {
            let Some((inflight, reply)) = self.in_flight.take() else {
                return;
            };
            match settle(&self.snap, &inflight, &reply, ME, self.mismatches) {
                SettleResult::Ops(ops) => {
                    self.mismatches = 0;
                    self.apply(&ops);
                }
                SettleResult::FetchServerCopy => {
                    self.mismatches += 1;
                    let ops = apply_server_copy(&self.snap, &server.envelope(server.seq()), ME);
                    self.apply(&ops);
                }
                SettleResult::Retry { mismatch, .. } => {
                    if mismatch {
                        self.mismatches += 1;
                    }
                }
            }
        }

        fn lose_reply(&mut self) {
            if self.in_flight.take().is_some() {
                self.must_catch_up = true;
            }
        }
    }

    fn token(n: u32) -> Value {
        json!(format!("t{n}"))
    }

    fn check_invariants(seed: u64, client: &Client) {
        let items = client.snap.content["items"]
            .as_array()
            .expect("items array");
        for n in 1..=client.local_edits {
            let copies = items.iter().filter(|item| **item == token(n)).count();
            assert!(
                copies <= 1,
                "seed {seed}: t{n} applied {copies} times: {items:?}"
            );
        }
        if client.snap.rows.is_empty() {
            let shadow = client.snap.shadow.as_ref().expect("shadow");
            assert_eq!(
                content_hash(&shadow.content),
                content_hash(&client.snap.content),
                "seed {seed}: local edit left without an outbox row"
            );
        }
    }

    fn drain(seed: u64, server: &mut Server, client: &mut Client, rng: &mut Jitter) {
        for _ in 0..50 {
            client.reply(server);
            client.catch_up(server, rng);
            check_invariants(seed, client);
            if client.snap.rows.is_empty() {
                return;
            }
            client.build(server, rng);
        }
        panic!("seed {seed}: rows never settled: {:?}", client.snap.rows);
    }

    #[test]
    fn random_echo_rebase_and_page_sequences_lose_and_duplicate_nothing() {
        for seed in 1..=SEEDS {
            let mut rng = Jitter::new(seed * 104_729);
            let start = json!({"items": [], "theirs": 0});
            let mut server = Server::new(start.clone());
            let mut client = Client::new(start);
            for _ in 0..STEPS {
                match (rng.next_unit() * 7.0) as u32 {
                    0 => client.edit(),
                    1 => client.build(&mut server, &mut rng),
                    2 => server.other_device_edit(),
                    3 => client.deliver(&server, false),
                    4 => client.deliver(&server, true),
                    5 => client.reply(&server),
                    _ => client.lose_reply(),
                }
                check_invariants(seed, &client);
            }
            drain(seed, &mut server, &mut client, &mut rng);

            assert_eq!(
                &client.snap.content,
                server.current(),
                "seed {seed}: client and server differ"
            );
            let final_items = server.current()["items"].as_array().unwrap();
            for n in 1..=client.local_edits {
                let kept = final_items.contains(&token(n))
                    || client.recovered.iter().any(|content| {
                        content["items"]
                            .as_array()
                            .is_some_and(|items| items.contains(&token(n)))
                    });
                assert!(kept, "seed {seed}: t{n} lost");
            }
        }
    }
}
