use json_patch::PatchOperation;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::hash::content_hash;
use super::list_merge::{ListMergeConfig, ListMergePolicy};
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
    /// The greatest upload id a pending row was sent in and that is not yet acknowledged: that
    /// upload may already be applied on the server.
    pub unacked_upload: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverReason {
    DeleteWins,
    Conflict,
    BecamePublication,
    CreateRejected,
    DeleteSuperseded,
    DeleteRefused,
    DeletePublication,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DocEvent {
    ConflictDetected,
    /// Only these paths collided with another device's change; the local values are kept aside.
    FieldConflict {
        paths: Vec<String>,
    },
    SyncError {
        code: String,
    },
    /// Another device changed the document after this data dir deleted it: the delete was
    /// dropped and the newer version kept; local edits made before the delete are set aside as
    /// a kept copy.
    DeleteSuperseded,
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
    /// Clears a local soft delete: the document is visible again.
    Undelete,
    RecordTombstone(Seq),
    SetMembership(Membership),
    Recover {
        content: Value,
        reason: RecoverReason,
    },
    /// Keeps the local values at colliding paths (and the full local content) in `recovered`
    /// with reason `field_conflict`.
    RecoverFields {
        content: Value,
        fields: Vec<FieldConflict>,
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
                DocOp::DeleteRows(ids) => {
                    s.rows.retain(|r| !ids.contains(&r.mutation_id));
                    s.forget_settled_upload();
                }
                DocOp::DropAllRows => {
                    s.rows.clear();
                    s.unacked_upload = None;
                }
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
                DocOp::Undelete => s.soft_deleted = false,
                DocOp::RecordTombstone(seq) => s.tombstone_seq = Some(*seq),
                DocOp::SetMembership(mm) => {
                    match s.memberships.iter_mut().find(|x| x.scope == mm.scope) {
                        Some(existing) => *existing = mm.clone(),
                        None => s.memberships.push(mm.clone()),
                    }
                }
                DocOp::Recover { .. } | DocOp::RecoverFields { .. } | DocOp::Emit(_) => {}
            }
        }
        s
    }

    /// A sent upload covers the rows up to its id; once none of them is left it is settled.
    fn forget_settled_upload(&mut self) {
        if let Some(upload_id) = self.unacked_upload {
            if !self.rows.iter().any(|r| r.mutation_id <= upload_id) {
                self.unacked_upload = None;
            }
        }
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

/// Non-echo upsert: content guard already passed. A rebase of pending rows onto the incoming
/// envelope is only attempted when a shadow exists and the caller says it is that base
/// (`local_base_known`); with no shadow at all there is no trustworthy base to rebase from — the
/// envelope may already hold our own upload whose reply was lost, or another device's edit the
/// shadow never captured. With no shadow, an envelope whose content equals the local content is
/// adopted; anything else settles as a conflict, same as when `local_base_known` is false.
fn apply_upsert(
    snap: &DocSnapshot,
    doc: &DocEnvelope,
    seq: Seq,
    local_base_known: bool,
    lists: &ListMergeConfig,
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
        if delete_pending(snap) {
            // A publication cannot be deleted through sync: it stays visible and read-only,
            // like every subscriber's copy, and the pending delete is dropped. Local content
            // is kept only if the publication replaced it and the user had edited it.
            let local = content_hash(&snap.content);
            let edited = content_hash(&doc.content) != local
                && snap
                    .shadow
                    .as_ref()
                    .is_none_or(|shadow| content_hash(&shadow.content) != local);
            if edited {
                ops.push(DocOp::Recover {
                    content: snap.content.clone(),
                    reason: RecoverReason::DeletePublication,
                });
            }
            ops.push(DocOp::DropAllRows);
            ops.push(DocOp::Undelete);
            ops.push(DocOp::SetShadow(new_shadow));
            ops.push(DocOp::SetContent(doc.content.clone()));
            if edited {
                ops.push(DocOp::Emit(DocEvent::SyncError {
                    code: "became_publication".into(),
                }));
            }
            return ops;
        }
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
    // A delete made on an older version must not destroy another device's newer edit. An
    // envelope holding the version it was made on (the shadow's, or our own last upload's)
    // leaves it pending, to go out on the new base. With no shadow the version it was made on
    // is unknown, so any content other than the local content supersedes it, and the local
    // content is kept whenever it differs.
    if delete_pending(snap) {
        let incoming = content_hash(&doc.content);
        let local = content_hash(&snap.content);
        let base = snap.shadow.as_ref().map(|old| content_hash(&old.content));
        if incoming != local && base.as_ref() != Some(&incoming) {
            if base.as_ref() != Some(&local) {
                ops.push(DocOp::Recover {
                    content: snap.content.clone(),
                    reason: RecoverReason::DeleteSuperseded,
                });
            }
            ops.push(DocOp::DropAllRows);
            ops.push(DocOp::Undelete);
            ops.push(DocOp::SetShadow(new_shadow));
            ops.push(DocOp::SetContent(doc.content.clone()));
            ops.push(DocOp::Emit(DocEvent::DeleteSuperseded));
            return ops;
        }
        ops.push(DocOp::SetShadow(new_shadow));
        return ops;
    }
    // No base, but the envelope already holds exactly our content (a create whose reply was
    // lost): adopt it. Both sides are hashed locally, never against the server's hash.
    if snap.shadow.is_none() && content_hash(&snap.content) == content_hash(&doc.content) {
        ops.push(DocOp::SetShadow(new_shadow));
        ops.push(DocOp::DropAllRows);
        return ops;
    }
    let rebased = match &snap.shadow {
        Some(old) if local_base_known => rebase(&old.content, &doc.content, &snap.content, lists),
        _ => Rebased::Conflict,
    };
    match rebased {
        Rebased::Clean(content) => {
            ops.push(DocOp::SetShadow(new_shadow));
            ops.push(DocOp::SetContent(content));
        }
        Rebased::Fields { content, conflicts } => {
            let paths = conflicts.iter().map(|c| c.path.clone()).collect();
            ops.push(DocOp::RecoverFields {
                content: snap.content.clone(),
                fields: conflicts,
            });
            ops.push(DocOp::SetShadow(new_shadow));
            // Rows stay: they still describe the non-colliding local edits (an empty diff
            // settles them at the next build) and keep any sent marks.
            ops.push(DocOp::SetContent(content));
            ops.push(DocOp::Emit(DocEvent::FieldConflict { paths }));
        }
        Rebased::Conflict => {
            let with_new_shadow = snap.project(&[DocOp::SetShadow(new_shadow.clone())]);
            ops.push(DocOp::SetShadow(new_shadow));
            ops.extend(conflict_ops(&with_new_shadow));
        }
    }
    ops
}

pub fn apply_change(
    snap: &DocSnapshot,
    change: &Change,
    me: Uuid,
    lists: &ListMergeConfig,
) -> Vec<DocOp> {
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
                    ops.extend(apply_upsert(&acked, doc, doc.seq, false, lists));
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
                    ops.extend(apply_upsert(
                        snap,
                        doc,
                        doc.seq.max(change.seq),
                        true,
                        lists,
                    ));
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

pub fn apply_server_copy(
    snap: &DocSnapshot,
    doc: &DocEnvelope,
    _me: Uuid,
    lists: &ListMergeConfig,
) -> Vec<DocOp> {
    if doc.seq <= snap.server_seq() {
        return Vec::new();
    }
    apply_upsert(snap, doc, doc.seq, true, lists)
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

/// Full resync: a local member of `scope` that the snapshot did not list has left it. A
/// document with outbox rows, or whose membership is newer than the snapshot, is kept.
pub fn sweep_doc(snap: &DocSnapshot, scope: &str, snapshot_seq: Seq) -> Vec<DocOp> {
    if !snap.rows.is_empty() || membership_seq(snap, scope) > snapshot_seq {
        return Vec::new();
    }
    let mut ops = vec![DocOp::SetMembership(Membership {
        scope: scope.to_string(),
        member: false,
        seq: snapshot_seq,
    })];
    let after = snap.project(&ops);
    if after.exists && !after.memberships.iter().any(|m| m.member) {
        ops.push(DocOp::HardDelete);
    }
    ops
}

/// A full-resync snapshot document. It carries no `upload_id`, so it may already contain a sent
/// upload of ours whose reply was lost: pending rows rebase onto it only when none of them was
/// sent without acknowledgement (or the shadow is a migrated v1 base at seq 0); otherwise they go
/// to `recovered`.
pub fn apply_snapshot_doc(
    snap: &DocSnapshot,
    scope: &str,
    doc: &DocEnvelope,
    me: Uuid,
    lists: &ListMergeConfig,
) -> Vec<DocOp> {
    let mut ops = Vec::new();
    if doc.seq > membership_seq(snap, scope) {
        ops.push(DocOp::SetMembership(Membership {
            scope: scope.to_string(),
            member: true,
            seq: doc.seq,
        }));
    }
    if doc.seq <= snap.server_seq() {
        return ops;
    }
    let member = snap
        .project(&ops)
        .memberships
        .iter()
        .any(|m| m.scope == scope && m.member);
    if member {
        // Rows never sent cannot be in the snapshot, so they rebase onto it from the shadow. A
        // sent, unacknowledged upload may already be applied there, so its rows conflict.
        let migrated_v1_base = snap.shadow.as_ref().is_some_and(|s| s.seq == 0);
        let base_known = migrated_v1_base || snap.unacked_upload.is_none();
        ops.extend(apply_upsert(snap, doc, doc.seq, base_known, lists));
    }
    with_settle_invariant(snap, ops, me)
}

/// A path both sides changed to different values: the server's value is kept, this is the
/// local side's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldConflict {
    /// JSON Pointer.
    pub path: String,
    /// `Null` when `local_removed`.
    pub local_value: Value,
    pub local_removed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Rebased {
    Clean(Value),
    /// Every local change applied except those at `conflicts`, where the server's value stays.
    Fields {
        content: Value,
        conflicts: Vec<FieldConflict>,
    },
    /// No sound merge: the whole document collided, or the kept local changes do not apply.
    Conflict,
}

/// Replays the local change (`old_base` → `local`) onto `new_base` field by field. A local
/// operation collides with a server one when their `collision_path`s are equal or one contains
/// the other and the two sides end with different values there; the collision is recorded at
/// the shorter path. A list that `lists` makes one value collides whole: the server's list is
/// kept and the local one set aside.
pub fn rebase(
    old_base: &Value,
    new_base: &Value,
    local: &Value,
    lists: &ListMergeConfig,
) -> Rebased {
    let local_ops = json_patch::diff(old_base, local).0;
    let lift = |path: &str| collision_path(old_base, local, new_base, path, lists);
    let their_paths: Vec<String> = json_patch::diff(old_base, new_base)
        .0
        .iter()
        .map(|op| lift(op_path(op)))
        .collect();
    let mut contested: Vec<String> = Vec::new();
    for op in &local_ops {
        let path = lift(op_path(op));
        let shortest = their_paths
            .iter()
            .filter(|theirs| contains(theirs, &path) || contains(&path, theirs))
            .map(|theirs| {
                if theirs.len() < path.len() {
                    theirs.clone()
                } else {
                    path.clone()
                }
            })
            .min_by_key(|p| p.len());
        if let Some(p) = shortest {
            if !contested.contains(&p) {
                contested.push(p);
            }
        }
    }
    let outermost: Vec<String> = contested
        .iter()
        .filter(|p| !contested.iter().any(|q| q != *p && contains(q, p)))
        .cloned()
        .collect();
    if outermost.iter().any(|p| p.is_empty()) {
        if local == new_base {
            return Rebased::Clean(new_base.clone());
        }
        return Rebased::Conflict;
    }
    let conflicts: Vec<FieldConflict> = outermost
        .iter()
        .filter(|p| local.pointer(p) != new_base.pointer(p))
        .map(|p| FieldConflict {
            path: p.clone(),
            local_value: local.pointer(p).cloned().unwrap_or(Value::Null),
            local_removed: local.pointer(p).is_none(),
        })
        .collect();
    let kept: Vec<PatchOperation> = local_ops
        .into_iter()
        .filter(|op| !outermost.iter().any(|p| contains(p, op_path(op))))
        .collect();
    let mut out = new_base.clone();
    if json_patch::patch(&mut out, &kept).is_err() {
        return Rebased::Conflict;
    }
    if conflicts.is_empty() {
        Rebased::Clean(out)
    } else {
        Rebased::Fields {
            content: out,
            conflicts,
        }
    }
}

fn op_path(op: &PatchOperation) -> &str {
    match op {
        PatchOperation::Add(op) => &op.path,
        PatchOperation::Remove(op) => &op.path,
        PatchOperation::Replace(op) => &op.path,
        PatchOperation::Move(op) => &op.path,
        PatchOperation::Copy(op) => &op.path,
        PatchOperation::Test(op) => &op.path,
    }
}

/// `path` is `ancestor` or lies under it ("" contains every path).
fn contains(ancestor: &str, path: &str) -> bool {
    path == ancestor
        || (path.len() > ancestor.len()
            && path.starts_with(ancestor)
            && path.as_bytes()[ancestor.len()] == b'/')
}

/// How one side left a list the old base had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListShape {
    SameLength,
    /// The old list is a prefix: elements were only added at the end.
    Appended,
    /// An insert or a removal, or the value is gone or no longer a list.
    Moved,
}

fn list_shape(old: &[Value], side: Option<&Value>) -> ListShape {
    match side {
        Some(Value::Array(now)) if now.len() == old.len() => ListShape::SameLength,
        Some(Value::Array(now)) if now.len() > old.len() && now[..old.len()] == old[..] => {
            ListShape::Appended
        }
        _ => ListShape::Moved,
    }
}

/// The indices where `side` differs from `old` (same length as `old`).
fn changed_indices(old: &[Value], side: &[Value]) -> std::collections::HashSet<usize> {
    old.iter()
        .zip(side)
        .enumerate()
        .filter_map(|(i, (was, now))| (was != now).then_some(i))
        .collect()
}

/// Whether any two elements of `values` are equal (deep equality for objects/arrays).
fn has_repeated_values(values: &[Value]) -> bool {
    values
        .iter()
        .enumerate()
        .any(|(i, v)| values[i + 1..].contains(v))
}

/// Whether the list at `path` collides as one value: always under `Atomic`; under `Append`
/// once positions stop lining up, or when the base list has a repeated value and both sides
/// changed it — with a repeat, "which twin" is ambiguous, so a disjoint-looking edit may
/// really be retargeted at the other side's twin. A list changed on only one side, or a
/// same-length edit on both sides at disjoint indices with no repeat, merges by position.
fn merges_as_one_value(
    old_base: &Value,
    local: &Value,
    new_base: &Value,
    path: &str,
    lists: &ListMergeConfig,
) -> bool {
    let Some(Value::Array(old)) = old_base.pointer(path) else {
        return false;
    };
    match lists.policy_for(path) {
        ListMergePolicy::Atomic | ListMergePolicy::Full => true,
        ListMergePolicy::Append => {
            let old_value = old_base.pointer(path);
            let local_value = local.pointer(path);
            let their_value = new_base.pointer(path);
            let both_changed = local_value != old_value && their_value != old_value;
            if both_changed && local_value != their_value && has_repeated_values(old) {
                return true;
            }
            match (list_shape(old, local_value), list_shape(old, their_value)) {
                (ListShape::SameLength, ListShape::SameLength) => {
                    let (Some(Value::Array(local)), Some(Value::Array(theirs))) =
                        (local_value, their_value)
                    else {
                        unreachable!("ListShape::SameLength implies an array of the same length")
                    };
                    !changed_indices(old, local).is_disjoint(&changed_indices(old, theirs))
                        && local != theirs
                }
                (ListShape::SameLength, ListShape::Appended)
                | (ListShape::Appended, ListShape::SameLength) => false,
                _ => true,
            }
        }
    }
}

/// The path a change at `path` collides at: the outermost list holding it that collides as
/// one value, else `path` itself.
fn collision_path(
    old_base: &Value,
    local: &Value,
    new_base: &Value,
    path: &str,
    lists: &ListMergeConfig,
) -> String {
    let mut prefix = String::new();
    if merges_as_one_value(old_base, local, new_base, &prefix, lists) {
        return prefix;
    }
    for token in path.split('/').skip(1) {
        prefix.push('/');
        prefix.push_str(token);
        if merges_as_one_value(old_base, local, new_base, &prefix, lists) {
            return prefix;
        }
    }
    path.to_string()
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use serde_json::json;

    pub const ME: Uuid = Uuid::from_u128(0xA);
    pub const OTHER: Uuid = Uuid::from_u128(0xB);
    pub const DOC: Uuid = Uuid::from_u128(0xD0C);

    /// The default list merge configuration: `Append`, no rules.
    pub static APPEND: ListMergeConfig = ListMergeConfig {
        default: ListMergePolicy::Append,
        rules: Vec::new(),
    };

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
            unacked_upload: None,
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
        let ops = apply_change(&s, &upsert("own", json!({"old": true}), 8), ME, &APPEND);
        assert!(!ops
            .iter()
            .any(|o| matches!(o, DocOp::SetContent(_) | DocOp::SetShadow(_))));
    }

    #[test]
    fn upsert_without_rows_replaces_content_and_shadow() {
        let s = synced(sample(), 1);
        let new = json!({"title": "B"});
        let after = s.project(&apply_change(
            &s,
            &upsert("own", new.clone(), 2),
            ME,
            &APPEND,
        ));
        assert_eq!(after.content, new);
        assert_eq!(after.shadow.unwrap().seq, 2);
    }

    #[test]
    fn page_upsert_stores_the_envelope_seq_so_older_page_changes_skip() {
        let s = synced(json!({"a": 1}), 1);
        // A page row for seq 2 carrying the document's current envelope (seq 5).
        let mut first = upsert("own", json!({}), 2);
        first.doc = Some(env(json!({"a": 3}), 5));
        let after = s.project(&apply_change(&s, &first, ME, &APPEND));
        assert_eq!(after.shadow.as_ref().unwrap().seq, 5);
        assert_eq!(after.content, json!({"a": 3}));

        let mut second = upsert("own", json!({}), 4);
        second.doc = Some(env(json!({"a": 3}), 5));
        let ops = apply_change(&after, &second, ME, &APPEND);
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
            &APPEND,
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
            &APPEND,
        ));
        assert_eq!(after.content, json!({"a": 1, "mine": true, "theirs": true}));
        assert_eq!(
            after.shadow.unwrap().content,
            json!({"a": 1, "theirs": true})
        );
        assert_eq!(after.rows.len(), 1);
    }

    #[test]
    fn rebase_field_conflict_keeps_the_local_value_aside_and_takes_the_server_value() {
        let mut s = synced(json!({"a": {"x": 1}}), 1);
        s.content = json!({"a": {"x": 2}});
        s.rows = vec![row(5, RowKind::Update)];
        let ops = apply_change(&s, &upsert("own", json!({}), 2), ME, &APPEND);
        assert!(ops.contains(&DocOp::RecoverFields {
            content: json!({"a": {"x": 2}}),
            fields: vec![FieldConflict {
                path: "/a".into(),
                local_value: json!({"x": 2}),
                local_removed: false
            }]
        }));
        assert!(ops.contains(&DocOp::Emit(DocEvent::FieldConflict {
            paths: vec!["/a".into()]
        })));
        assert!(!ops.iter().any(|op| matches!(op, DocOp::Recover { .. })));
        let after = s.project(&ops);
        assert_eq!(after.content, json!({}));
        assert_eq!(
            after.rows.len(),
            1,
            "rows stay; the next build settles an empty diff"
        );
    }

    #[test]
    fn field_conflict_keeps_non_colliding_local_edits_pending() {
        let mut s = synced(json!({"s": "old", "k": 1}), 1);
        s.content = json!({"s": "mine", "k": 2});
        s.rows = vec![row(5, RowKind::Update)];
        let ops = apply_change(
            &s,
            &upsert("own", json!({"s": "theirs", "k": 1}), 2),
            ME,
            &APPEND,
        );
        let after = s.project(&ops);
        assert_eq!(after.content, json!({"s": "theirs", "k": 2}));
        assert_eq!(
            after.shadow.unwrap().content,
            json!({"s": "theirs", "k": 1})
        );
        assert_eq!(after.rows.len(), 1);
    }

    #[test]
    fn an_upsert_that_changed_a_pending_delete_supersedes_it() {
        let mut s = synced(json!({"a": {"x": 1}}), 1);
        s.content = json!({"a": {"x": 2}});
        s.soft_deleted = true;
        s.rows = vec![row(5, RowKind::Update), row(6, RowKind::Delete)];
        let ops = apply_change(&s, &upsert("own", json!({"a": {"x": 3}}), 2), ME, &APPEND);
        assert!(ops.contains(&DocOp::Emit(DocEvent::DeleteSuperseded)));
        assert!(ops.contains(&DocOp::Recover {
            content: json!({"a": {"x": 2}}),
            reason: RecoverReason::DeleteSuperseded
        }));
        assert!(!ops
            .iter()
            .any(|op| matches!(op, DocOp::RecoverFields { .. })));
        let after = s.project(&ops);
        assert!(after.exists && !after.soft_deleted);
        assert!(after.rows.is_empty());
        assert_eq!(after.content, json!({"a": {"x": 3}}));
        assert_eq!(after.shadow.unwrap().seq, 2);
    }

    #[test]
    fn an_upsert_holding_the_deleted_version_keeps_the_delete() {
        let mut s = synced(json!({"a": 1}), 1);
        s.soft_deleted = true;
        s.rows = vec![row(6, RowKind::Delete)];
        let ops = apply_change(&s, &upsert("own", json!({"a": 1}), 2), ME, &APPEND);
        assert!(!ops
            .iter()
            .any(|op| matches!(op, DocOp::Emit(_) | DocOp::Undelete)));
        let after = s.project(&ops);
        assert!(after.soft_deleted);
        assert_eq!(after.rows, s.rows);
        assert_eq!(
            after.shadow.unwrap().seq,
            2,
            "the delete goes out on the new base"
        );
    }

    #[test]
    fn an_upsert_holding_our_own_last_edit_keeps_the_delete() {
        // Our update landed but its reply was lost; then the user deleted the document.
        let mut s = synced(json!({"a": 1}), 1);
        s.content = json!({"a": 2});
        s.soft_deleted = true;
        s.rows = vec![row(5, RowKind::Update), row(6, RowKind::Delete)];
        let ops = apply_change(&s, &upsert("own", json!({"a": 2}), 2), ME, &APPEND);
        assert!(!ops
            .iter()
            .any(|op| matches!(op, DocOp::Emit(_) | DocOp::Undelete)));
        let after = s.project(&ops);
        assert!(after.soft_deleted);
        assert_eq!(after.rows, s.rows);
        assert_eq!(after.shadow.unwrap().content, json!({"a": 2}));
    }

    #[test]
    fn a_server_copy_that_changed_a_pending_delete_supersedes_it() {
        let mut s = synced(json!({"a": 1}), 1);
        s.soft_deleted = true;
        s.rows = vec![row(6, RowKind::Delete)];
        let ops = apply_server_copy(&s, &env(json!({"a": 3}), 4), ME, &APPEND);
        assert!(ops.contains(&DocOp::Emit(DocEvent::DeleteSuperseded)));
        assert!(
            !ops.iter().any(|op| matches!(op, DocOp::Recover { .. })),
            "an unedited document has nothing to keep"
        );
        let after = s.project(&ops);
        assert!(!after.soft_deleted && after.rows.is_empty());
        assert_eq!(after.content, json!({"a": 3}));
    }

    #[test]
    fn a_pending_delete_without_a_shadow_is_superseded_by_other_content() {
        let mut s = synced(json!({"a": 1}), 0);
        s.shadow = None;
        s.soft_deleted = true;
        s.rows = vec![row(1, RowKind::Create), row(2, RowKind::Delete)];
        let ops = apply_change(&s, &upsert("own", json!({"a": 9}), 3), ME, &APPEND);
        assert!(ops.contains(&DocOp::Emit(DocEvent::DeleteSuperseded)));
        assert!(
            ops.contains(&DocOp::Recover {
                content: json!({"a": 1}),
                reason: RecoverReason::DeleteSuperseded
            }),
            "with no shadow, any local content other than the incoming is kept"
        );
        let after = s.project(&ops);
        assert!(after.exists && !after.soft_deleted);
        assert!(after.rows.is_empty());
        assert_eq!(after.content, json!({"a": 9}));
        assert_eq!(after.shadow.unwrap().seq, 3);
    }

    #[test]
    fn a_delete_without_a_shadow_is_superseded_by_a_version_this_device_never_saw() {
        // Create sent, reply lost; another device edited it; this device deleted it offline
        // and returns via a full resync.
        let mut s = synced(json!({"a": 1}), 0);
        s.shadow = None;
        s.soft_deleted = true;
        s.rows = vec![row(1, RowKind::Create), row(2, RowKind::Delete)];
        s.unacked_upload = Some(m(1));
        let other_device = env(json!({"a": 9}), 5);
        let ops = apply_snapshot_doc(&s, "own", &other_device, ME, &APPEND);
        assert!(ops.contains(&DocOp::Emit(DocEvent::DeleteSuperseded)));
        assert!(ops.contains(&DocOp::Recover {
            content: json!({"a": 1}),
            reason: RecoverReason::DeleteSuperseded
        }));
        let after = s.project(&ops);
        assert!(after.exists && !after.soft_deleted);
        assert_eq!(after.content, json!({"a": 9}));
        assert_eq!(
            build_upload(&after, ME),
            BuildResult::Nothing,
            "no delete goes out on the other device's version"
        );
    }

    #[test]
    fn a_delete_without_a_shadow_stays_pending_when_the_server_holds_our_content() {
        // Our create landed but its reply was lost; the envelope is that create.
        let mut s = synced(json!({"a": 1}), 0);
        s.shadow = None;
        s.soft_deleted = true;
        s.rows = vec![row(1, RowKind::Create), row(2, RowKind::Delete)];
        s.unacked_upload = Some(m(1));
        let ours = env(json!({"a": 1}), 5);
        let ops = apply_snapshot_doc(&s, "own", &ours, ME, &APPEND);
        assert!(!ops
            .iter()
            .any(|op| matches!(op, DocOp::Emit(_) | DocOp::Undelete | DocOp::Recover { .. })));
        let after = s.project(&ops);
        assert!(after.soft_deleted);
        assert_eq!(after.rows, s.rows);
        assert_eq!(after.shadow.as_ref().unwrap().seq, 5);
        match build_upload(&after, ME) {
            BuildResult::Send { upload, .. } => {
                assert_eq!(upload.kind, crate::engine::types::UploadKind::Delete);
                assert_eq!(
                    upload.base_hash,
                    Some(ours.hash),
                    "the delete goes out on our version"
                );
            }
            other => panic!("expected the delete, got {other:?}"),
        }
    }

    #[test]
    fn null_shadow_with_pending_rows_recovers_exact_local_content() {
        // No known base (a v2 create whose reply was lost, or a migrated v1 doc with no
        // captured base): rebasing risks re-applying or silently dropping content, so this
        // settles as a conflict instead of keeping local content unconditionally.
        let mut s = synced(json!({"items": ["a"]}), 0);
        s.shadow = None;
        s.rows = vec![row(1, RowKind::Create)];
        let ops = apply_change(
            &s,
            &upsert("own", json!({"items": ["a", "x"]}), 4),
            ME,
            &APPEND,
        );
        assert!(ops.contains(&DocOp::Recover {
            content: json!({"items": ["a"]}),
            reason: RecoverReason::Conflict,
        }));
        assert!(ops.contains(&DocOp::Emit(DocEvent::ConflictDetected)));
        let after = s.project(&ops);
        assert_eq!(after.content, json!({"items": ["a", "x"]}));
        assert!(after.rows.is_empty());
        assert_eq!(after.shadow.unwrap().seq, 4);
    }

    #[test]
    fn membership_guard_is_independent_of_content_guard() {
        // Doc already at server_seq 20; lagging scope Y delivers upsert@15.
        let s = synced(sample(), 20);
        let ops = apply_change(&s, &upsert("collection:y", sample(), 15), ME, &APPEND);
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
        let ops = apply_change(&s, &upsert("collection:y", sample(), 15), ME, &APPEND);
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
            &APPEND,
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
            &APPEND,
        );
        let after_leave = s.project(&leave_ops);
        let after = after_leave.project(&apply_change(
            &after_leave,
            &upsert("collection:curated", sample(), 3),
            ME,
            &APPEND,
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
            &APPEND,
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
        let ops = apply_change(&s, &c, ME, &APPEND);
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
    fn becoming_a_publication_over_an_unedited_pending_delete_unhides_it_without_a_kept_copy() {
        let mut s = synced(json!({"a": 1}), 1);
        s.soft_deleted = true;
        s.rows = vec![row(5, RowKind::Delete)];
        let mut c = upsert("collection:curated", json!({"a": 2}), 2);
        c.doc.as_mut().unwrap().read_only = true;
        let ops = apply_change(&s, &c, ME, &APPEND);
        assert!(
            !ops.iter()
                .any(|op| matches!(op, DocOp::Recover { .. } | DocOp::Emit(_))),
            "unedited: nothing of the user's to keep, nothing to report"
        );
        let after = s.project(&ops);
        assert!(after.exists && !after.soft_deleted && after.read_only);
        assert!(after.rows.is_empty());
        assert_eq!(after.content, json!({"a": 2}));
    }

    #[test]
    fn becoming_a_publication_over_an_edited_pending_delete_keeps_the_edit() {
        let mut s = synced(json!({"a": 1}), 1);
        s.content = json!({"a": 9});
        s.soft_deleted = true;
        s.rows = vec![row(4, RowKind::Update), row(5, RowKind::Delete)];
        let mut c = upsert("collection:curated", json!({"a": 2}), 2);
        c.doc.as_mut().unwrap().read_only = true;
        let ops = apply_change(&s, &c, ME, &APPEND);
        assert!(ops.contains(&DocOp::Recover {
            content: json!({"a": 9}),
            reason: RecoverReason::DeletePublication
        }));
        assert!(ops.contains(&DocOp::Emit(DocEvent::SyncError {
            code: "became_publication".into()
        })));
        let after = s.project(&ops);
        assert!(after.exists && !after.soft_deleted && after.read_only);
        assert!(after.rows.is_empty());
        assert_eq!(after.content, json!({"a": 2}));
    }

    fn published(content: serde_json::Value, seq: i64) -> Change {
        let mut c = upsert("collection:curated", content, seq);
        c.doc.as_mut().unwrap().read_only = true;
        c
    }

    #[test]
    fn a_publication_equal_to_the_local_edit_keeps_no_copy() {
        let mut s = synced(json!({"a": 1}), 1);
        s.content = json!({"a": 9});
        s.soft_deleted = true;
        s.rows = vec![row(4, RowKind::Update), row(5, RowKind::Delete)];
        s.unacked_upload = Some(m(4));
        let ops = apply_change(&s, &published(json!({"a": 9}), 3), ME, &APPEND);
        assert!(
            !ops.iter()
                .any(|op| matches!(op, DocOp::Recover { .. } | DocOp::Emit(_))),
            "{ops:?}"
        );
        let after = s.project(&ops);
        assert!(after.exists && !after.soft_deleted && after.read_only);
        assert!(after.rows.is_empty());
    }

    #[test]
    fn a_parked_delete_of_a_never_shadowed_document_is_undone_by_a_publication() {
        let mut s = synced(json!({"a": 1}), 1);
        s.shadow = None;
        s.soft_deleted = true;
        let mut create = row(4, RowKind::Create);
        create.parked = true;
        let mut delete = row(5, RowKind::Delete);
        delete.parked = true;
        s.rows = vec![create, delete];
        let ops = apply_change(&s, &published(json!({"a": 1}), 3), ME, &APPEND);
        let after = s.project(&ops);
        assert!(after.exists && !after.soft_deleted && after.read_only && after.rows.is_empty());
        assert!(!ops
            .iter()
            .any(|op| matches!(op, DocOp::Recover { .. } | DocOp::Emit(_))));
        let ops = apply_change(&s, &published(json!({"a": 2}), 3), ME, &APPEND);
        assert!(ops.contains(&DocOp::Recover {
            content: json!({"a": 1}),
            reason: RecoverReason::DeletePublication
        }));
        assert_eq!(
            ops.iter().filter(|op| matches!(op, DocOp::Emit(_))).count(),
            1
        );
    }

    #[test]
    fn the_echo_of_our_own_update_as_a_publication_unhides_a_document_deleted_after() {
        let mut s = synced(json!({"a": 1}), 1);
        s.content = json!({"a": 9});
        s.soft_deleted = true;
        s.rows = vec![row(4, RowKind::Update), row(5, RowKind::Delete)];
        s.unacked_upload = Some(m(4));
        let mut c = published(json!({"a": 9}), 3);
        c.seq = 2;
        c.scope = "own".into();
        c.upload_id = Some(m(4));
        let ops = apply_change(&s, &c, ME, &APPEND);
        let after = s.project(&ops);
        assert!(
            after.exists && !after.soft_deleted && after.read_only && after.rows.is_empty(),
            "{ops:?}"
        );
    }

    #[test]
    fn a_server_copy_that_is_a_publication_over_a_never_shadowed_delete_keeps_the_local_content() {
        let mut s = synced(json!({"a": 1}), 1);
        s.shadow = None;
        s.soft_deleted = true;
        s.rows = vec![row(5, RowKind::Delete)];
        let mut e = env(json!({"a": 2}), 3);
        e.read_only = true;
        let ops = apply_server_copy(&s, &e, ME, &APPEND);
        let after = s.project(&ops);
        assert!(after.exists && !after.soft_deleted && after.read_only && after.rows.is_empty());
        assert!(ops.contains(&DocOp::Recover {
            content: json!({"a": 1}),
            reason: RecoverReason::DeletePublication
        }));
    }

    #[test]
    fn server_copy_applies_with_content_guard() {
        let s = synced(sample(), 5);
        assert!(apply_server_copy(&s, &env(json!({"x": 1}), 4), ME, &APPEND).is_empty());
        let after = s.project(&apply_server_copy(
            &s,
            &env(json!({"x": 1}), 6),
            ME,
            &APPEND,
        ));
        assert_eq!(after.content, json!({"x": 1}));
    }

    #[test]
    fn server_copy_with_no_shadow_and_pending_rows_recovers_exact_local_content() {
        let mut s = synced(json!({"items": ["a"]}), 0);
        s.shadow = None;
        s.rows = vec![row(1, RowKind::Create)];
        let ops = apply_server_copy(&s, &env(json!({"items": ["a", "x"]}), 4), ME, &APPEND);
        assert!(ops.contains(&DocOp::Recover {
            content: json!({"items": ["a"]}),
            reason: RecoverReason::Conflict,
        }));
        assert!(ops.contains(&DocOp::Emit(DocEvent::ConflictDetected)));
        let after = s.project(&ops);
        assert_eq!(after.content, json!({"items": ["a", "x"]}));
        assert!(after.rows.is_empty());
    }

    #[test]
    fn server_copy_field_conflict_with_a_sent_unacked_upload_keeps_rows_and_the_mark() {
        let mut s = synced(json!({"s": "old", "k": 1}), 1);
        s.content = json!({"s": "mine", "k": 2});
        s.rows = vec![row(5, RowKind::Update)];
        s.unacked_upload = Some(m(5));
        let ops = apply_server_copy(&s, &env(json!({"s": "theirs", "k": 1}), 2), ME, &APPEND);
        assert!(ops.contains(&DocOp::RecoverFields {
            content: json!({"s": "mine", "k": 2}),
            fields: vec![FieldConflict {
                path: "/s".into(),
                local_value: json!("mine"),
                local_removed: false
            }]
        }));
        let after = s.project(&ops);
        assert_eq!(after.content, json!({"s": "theirs", "k": 2}));
        assert_eq!(after.rows.len(), 1);
        assert_eq!(after.unacked_upload, Some(m(5)));
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
        let after = s.project(&apply_change(&s, &echo, ME, &APPEND));
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

        let ops = apply_change(&s, &page_echo(envelope.clone()), ME, &APPEND);
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

        let ops = apply_change(&s, &page_echo(envelope.clone()), ME, &APPEND);

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

        let ops = apply_change(&s, &page_echo(envelope.clone()), ME, &APPEND);

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

        let ops = apply_change(&s, &page_echo(envelope.clone()), ME, &APPEND);

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

        let ops = apply_change(&s, &page_echo(envelope.clone()), ME, &APPEND);

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

        let ops = apply_change(&s, &page_echo, ME, &APPEND);
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
            &APPEND,
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
            &APPEND,
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
            &APPEND,
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
            &APPEND,
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
        let ops = apply_change(&s, &kind_change("own", ChangeKind::Delete, 2), ME, &APPEND);
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
        let ops = apply_change(&s, &kind_change("own", ChangeKind::Delete, 2), ME, &APPEND);
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
            &APPEND,
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
        let ops = apply_change(&s, &upsert("collection:y", sample(), 15), ME, &APPEND);
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
        apply_change(&s, &kind_change("own", ChangeKind::Upsert, 2), ME, &APPEND);
    }
}

#[cfg(test)]
mod rebase_tests {
    use super::fixtures::APPEND;
    use super::*;
    use crate::engine::list_merge::{ListMergePolicy, PathPattern};
    use serde_json::json;

    #[test]
    fn independent_edits_merge() {
        let r = rebase(
            &json!({"a": 1}),
            &json!({"a": 1, "b": 2}),
            &json!({"a": 1, "c": 3}),
            &APPEND,
        );
        assert_eq!(r, Rebased::Clean(json!({"a": 1, "b": 2, "c": 3})));
    }

    #[test]
    fn append_to_an_array_the_server_made_an_object_collides_at_the_array() {
        let r = rebase(
            &json!({"items": ["a"]}),
            &json!({"items": {"k": 1}}),
            &json!({"items": ["a", "b"]}),
            &APPEND,
        );
        assert_eq!(
            r,
            Rebased::Fields {
                content: json!({"items": {"k": 1}}),
                conflicts: vec![kept("/items", json!(["a", "b"]))]
            }
        );
    }

    #[test]
    fn no_local_change_takes_new_base() {
        let r = rebase(
            &json!({"a": 1}),
            &json!({"a": 2}),
            &json!({"a": 1}),
            &APPEND,
        );
        assert_eq!(r, Rebased::Clean(json!({"a": 2})));
    }

    fn kept(path: &str, local_value: Value) -> FieldConflict {
        FieldConflict {
            path: path.into(),
            local_value,
            local_removed: false,
        }
    }

    #[test]
    fn edit_under_removed_parent_is_a_field_conflict() {
        let r = rebase(
            &json!({"a": {"x": 1}, "k": 1}),
            &json!({"k": 1}),
            &json!({"a": {"x": 2}, "k": 2}),
            &APPEND,
        );
        assert_eq!(
            r,
            Rebased::Fields {
                content: json!({"k": 2}),
                conflicts: vec![kept("/a", json!({"x": 2}))]
            }
        );
    }

    #[test]
    fn same_field_set_on_both_sides_is_a_field_conflict() {
        let r = rebase(
            &json!({"s": "old", "k": 1}),
            &json!({"s": "theirs", "k": 1}),
            &json!({"s": "mine", "k": 2}),
            &APPEND,
        );
        assert_eq!(
            r,
            Rebased::Fields {
                content: json!({"s": "theirs", "k": 2}),
                conflicts: vec![kept("/s", json!("mine"))]
            }
        );
    }

    #[test]
    fn same_value_on_both_sides_is_not_a_conflict() {
        let r = rebase(
            &json!({"s": "old"}),
            &json!({"s": "x"}),
            &json!({"s": "x"}),
            &APPEND,
        );
        assert_eq!(r, Rebased::Clean(json!({"s": "x"})));
    }

    #[test]
    fn local_remove_against_a_server_set_is_a_field_conflict() {
        let r = rebase(
            &json!({"s": "old", "k": 1}),
            &json!({"s": "theirs", "k": 1}),
            &json!({"k": 1}),
            &APPEND,
        );
        assert_eq!(
            r,
            Rebased::Fields {
                content: json!({"s": "theirs", "k": 1}),
                conflicts: vec![FieldConflict {
                    path: "/s".into(),
                    local_value: Value::Null,
                    local_removed: true
                }]
            }
        );
    }

    #[test]
    fn an_ancestor_and_a_descendant_change_collide_at_the_ancestor() {
        let r = rebase(
            &json!({"a": {"x": 1, "y": 1}}),
            &json!({"a": {"x": 2, "y": 1}}),
            &json!({"a": "flat"}),
            &APPEND,
        );
        assert_eq!(
            r,
            Rebased::Fields {
                content: json!({"a": {"x": 2, "y": 1}}),
                conflicts: vec![kept("/a", json!("flat"))]
            }
        );
    }

    #[test]
    fn appends_to_one_array_from_both_sides_collide_at_the_array() {
        let r = rebase(
            &json!({"items": ["a"]}),
            &json!({"items": ["a", "t"]}),
            &json!({"items": ["a", "m1", "m2"]}),
            &APPEND,
        );
        assert_eq!(
            r,
            Rebased::Fields {
                content: json!({"items": ["a", "t"]}),
                conflicts: vec![kept("/items", json!(["a", "m1", "m2"]))]
            }
        );
    }

    #[test]
    fn overlapping_array_elements_changed_on_both_sides_collide_at_the_array() {
        let r = rebase(
            &json!({"items": ["a", "b", "c"]}),
            &json!({"items": ["a", "B", "c"]}),
            &json!({"items": ["a", "b2", "C"]}),
            &APPEND,
        );
        assert_eq!(
            r,
            Rebased::Fields {
                content: json!({"items": ["a", "B", "c"]}),
                conflicts: vec![kept("/items", json!(["a", "b2", "C"]))]
            }
        );
    }

    #[test]
    fn array_diffs_are_positional() {
        // The collision rule treats array element paths as fields; that is only sound while
        // json_patch::diff compares arrays position by position.
        let patch = json_patch::diff(&json!(["a", "b", "c"]), &json!(["b", "c", "d", "e"]));
        assert_eq!(
            serde_json::to_value(&patch).unwrap(),
            json!([
                {"op": "replace", "path": "/0", "value": "b"},
                {"op": "replace", "path": "/1", "value": "c"},
                {"op": "replace", "path": "/2", "value": "d"},
                {"op": "add", "path": "/3", "value": "e"}
            ])
        );
    }

    fn pitches(list: Value) -> Value {
        json!({ "pitches": list })
    }

    fn scale() -> Value {
        pitches(json!([0, 200, 400, 500, 700, 900]))
    }

    /// The server's list is kept and the whole local list is set aside.
    fn list_kept_aside(theirs: Value, mine: Value) -> Rebased {
        Rebased::Fields {
            conflicts: vec![kept("/pitches", mine["pitches"].clone())],
            content: theirs,
        }
    }

    fn atomic() -> ListMergeConfig {
        ListMergeConfig {
            default: ListMergePolicy::Atomic,
            rules: Vec::new(),
        }
    }

    #[test]
    fn retunes_of_different_degrees_merge() {
        let mine = pitches(json!([0, 200, 386, 500, 700, 900]));
        let theirs = pitches(json!([0, 200, 400, 500, 700, 884]));
        assert_eq!(
            rebase(&scale(), &theirs, &mine, &APPEND),
            Rebased::Clean(pitches(json!([0, 200, 386, 500, 700, 884])))
        );
    }

    #[test]
    fn removals_on_both_sides_keep_the_servers_list() {
        let mine = pitches(json!([0, 200, 500, 700, 900]));
        let theirs = pitches(json!([0, 200, 400, 500, 700]));
        assert_eq!(
            rebase(&scale(), &theirs, &mine, &APPEND),
            list_kept_aside(theirs, mine)
        );
    }

    #[test]
    fn an_insert_against_a_retune_keeps_the_servers_list() {
        let mine = pitches(json!([0, 200, 400, 450, 500, 700, 900]));
        let theirs = pitches(json!([0, 200, 400, 500, 702, 900]));
        assert_eq!(
            rebase(&scale(), &theirs, &mine, &APPEND),
            list_kept_aside(theirs, mine)
        );
    }

    #[test]
    fn inserts_at_one_spot_on_both_sides_keep_the_servers_list() {
        let mine = pitches(json!([0, 200, 400, 450, 500, 700, 900]));
        let theirs = pitches(json!([0, 200, 400, 460, 500, 700, 900]));
        assert_eq!(
            rebase(&scale(), &theirs, &mine, &APPEND),
            list_kept_aside(theirs, mine)
        );
    }

    #[test]
    fn a_removal_against_a_retune_of_that_degree_keeps_the_servers_list() {
        let mine = pitches(json!([0, 200, 400, 700, 900]));
        let theirs = pitches(json!([0, 200, 400, 498, 700, 900]));
        assert_eq!(
            rebase(&scale(), &theirs, &mine, &APPEND),
            list_kept_aside(theirs, mine)
        );
    }

    #[test]
    fn a_removal_of_a_repeated_value_against_a_retune_of_its_twin_keeps_the_servers_list() {
        let old = pitches(json!([0, 200, 200, 500]));
        let mine = pitches(json!([0, 200, 500]));
        let theirs = pitches(json!([0, 204, 200, 500]));
        assert_eq!(
            rebase(&old, &theirs, &mine, &APPEND),
            list_kept_aside(theirs, mine)
        );
    }

    #[test]
    fn a_local_append_merges_with_a_server_retune() {
        let mine = pitches(json!([0, 200, 400, 500, 700, 900, 1000]));
        let theirs = pitches(json!([0, 200, 386, 500, 700, 900]));
        assert_eq!(
            rebase(&scale(), &theirs, &mine, &APPEND),
            Rebased::Clean(pitches(json!([0, 200, 386, 500, 700, 900, 1000])))
        );
    }

    #[test]
    fn a_server_append_merges_with_a_local_retune() {
        let mine = pitches(json!([0, 200, 386, 500, 700, 900]));
        let theirs = pitches(json!([0, 200, 400, 500, 700, 900, 1000]));
        assert_eq!(
            rebase(&scale(), &theirs, &mine, &APPEND),
            Rebased::Clean(pitches(json!([0, 200, 386, 500, 700, 900, 1000])))
        );
    }

    #[test]
    fn appends_on_both_sides_keep_the_servers_list() {
        let mine = pitches(json!([0, 200, 400, 500, 700, 900, 1000]));
        let theirs = pitches(json!([0, 200, 400, 500, 700, 900, 1100]));
        assert_eq!(
            rebase(&scale(), &theirs, &mine, &APPEND),
            list_kept_aside(theirs, mine)
        );
    }

    #[test]
    fn an_append_against_a_removal_keeps_the_servers_list() {
        let mine = pitches(json!([0, 200, 400, 500, 700, 900, 1000]));
        let theirs = pitches(json!([0, 200, 400, 500, 700]));
        assert_eq!(
            rebase(&scale(), &theirs, &mine, &APPEND),
            list_kept_aside(theirs, mine)
        );
    }

    #[test]
    fn the_same_append_on_both_sides_is_one_element() {
        let both = pitches(json!([0, 200, 400, 500, 700, 900, 1000]));
        assert_eq!(
            rebase(&scale(), &both, &both, &APPEND),
            Rebased::Clean(both.clone())
        );
    }

    fn two_tunings() -> Value {
        json!({"tunings": [
            {"name": "A", "pitches": [0, 100]},
            {"name": "B", "pitches": [0, 200]}
        ]})
    }

    /// Inserts 50 into the first tuning's pitches.
    fn inner_insert() -> Value {
        json!({"tunings": [
            {"name": "A", "pitches": [0, 50, 100]},
            {"name": "B", "pitches": [0, 200]}
        ]})
    }

    #[test]
    fn a_resized_inner_list_merges_with_edits_at_a_different_index_in_the_outer_list() {
        let renamed = json!({"tunings": [
            {"name": "A", "pitches": [0, 100]},
            {"name": "B2", "pitches": [0, 200]}
        ]});
        assert_eq!(
            rebase(&two_tunings(), &renamed, &inner_insert(), &APPEND),
            Rebased::Clean(json!({"tunings": [
                {"name": "A", "pitches": [0, 50, 100]},
                {"name": "B2", "pitches": [0, 200]}
            ]}))
        );
    }

    #[test]
    fn a_resized_inner_list_collides_with_an_edit_at_the_same_index_in_the_outer_list() {
        // Both sides touch tuning A (outer index 0), so the outer /tunings list collides whole
        // even though the edits (an insert into A's pitches, a retune of A's pitches) would
        // have merged if they had landed on different tunings.
        let retuned = json!({"tunings": [
            {"name": "A", "pitches": [0, 101]},
            {"name": "B", "pitches": [0, 200]}
        ]});
        assert_eq!(
            rebase(&two_tunings(), &retuned, &inner_insert(), &APPEND),
            Rebased::Fields {
                content: retuned.clone(),
                conflicts: vec![kept("/tunings", inner_insert()["tunings"].clone())]
            }
        );
    }

    #[test]
    fn a_resized_outer_list_collides_with_any_edit_inside_it() {
        let removed = json!({"tunings": [{"name": "A", "pitches": [0, 100]}]});
        assert_eq!(
            rebase(&two_tunings(), &removed, &inner_insert(), &APPEND),
            Rebased::Fields {
                content: removed.clone(),
                conflicts: vec![kept("/tunings", inner_insert()["tunings"].clone())]
            }
        );
    }

    #[test]
    fn retunes_of_different_degrees_conflict_when_the_list_is_atomic() {
        let mine = pitches(json!([0, 200, 386, 500, 700, 900]));
        let theirs = pitches(json!([0, 200, 400, 500, 700, 884]));
        assert_eq!(
            rebase(&scale(), &theirs, &mine, &atomic()),
            list_kept_aside(theirs, mine)
        );
    }

    #[test]
    fn a_full_policy_that_bypassed_start_rebases_like_atomic() {
        let full = ListMergeConfig {
            default: ListMergePolicy::Full,
            rules: Vec::new(),
        };
        let mine = pitches(json!([0, 200, 386, 500, 700, 900]));
        let theirs = pitches(json!([0, 200, 400, 500, 700, 884]));
        assert_eq!(
            rebase(&scale(), &theirs, &mine, &full),
            list_kept_aside(theirs, mine)
        );
    }

    #[test]
    fn an_atomic_list_changed_on_one_side_applies() {
        let old = json!({"name": "12-TET", "pitches": [0, 200, 400]});
        let mine = json!({"name": "12-TET", "pitches": [0, 200, 386, 1000]});
        let theirs = json!({"name": "Just", "pitches": [0, 200, 400]});
        assert_eq!(
            rebase(&old, &theirs, &mine, &atomic()),
            Rebased::Clean(json!({"name": "Just", "pitches": [0, 200, 386, 1000]}))
        );
    }

    // Tunings keyed by name (an object, not a list) so the two edits below land in the same
    // pitches list without the outer container itself being a list rule (b) also governs.
    fn tunings_by_name() -> Value {
        json!({"tunings": {
            "A": {"pitches": [0, 100]},
            "B": {"pitches": [0, 200]}
        }})
    }

    #[test]
    fn rules_choose_the_policy_per_list() {
        let mine = json!({"tunings": {
            "A": {"pitches": [0, 101]},
            "B": {"pitches": [0, 200]}
        }});
        let theirs = json!({"tunings": {
            "A": {"pitches": [1, 100]},
            "B": {"pitches": [0, 200]}
        }});
        assert_eq!(
            rebase(&tunings_by_name(), &theirs, &mine, &APPEND),
            Rebased::Clean(json!({"tunings": {
                "A": {"pitches": [1, 101]},
                "B": {"pitches": [0, 200]}
            }}))
        );
        let atomic_pitches = ListMergeConfig {
            default: ListMergePolicy::Append,
            rules: vec![(
                PathPattern("/tunings/*/pitches".into()),
                ListMergePolicy::Atomic,
            )],
        };
        assert_eq!(
            rebase(&tunings_by_name(), &theirs, &mine, &atomic_pitches),
            Rebased::Fields {
                content: theirs.clone(),
                conflicts: vec![kept("/tunings/A/pitches", json!([0, 101]))]
            }
        );
    }

    #[test]
    fn a_removal_on_each_side_with_a_shared_append_collides_at_the_array() {
        // Mine removes 400 and appends 1200; theirs removes 700 and appends 1200. The two
        // removals shift the tail by one either way, so the shifted diffs land on overlapping
        // indices; a plain positional merge would silently undo theirs' removal of 700.
        let mine = pitches(json!([0, 200, 500, 700, 900, 1200]));
        let theirs = pitches(json!([0, 200, 400, 500, 900, 1200]));
        assert_eq!(
            rebase(&scale(), &theirs, &mine, &APPEND),
            list_kept_aside(theirs, mine)
        );
    }

    #[test]
    fn a_removal_on_each_side_with_a_shared_middle_insert_collides_at_the_array() {
        // Same shape as above, but the shared new value (800) lands in the middle of each list
        // rather than at the tail: mine removes 400 and inserts 800; theirs removes 700 and
        // inserts 800.
        let mine = pitches(json!([0, 200, 500, 700, 800, 900]));
        let theirs = pitches(json!([0, 200, 800, 400, 500, 900]));
        assert_eq!(
            rebase(&scale(), &theirs, &mine, &APPEND),
            list_kept_aside(theirs, mine)
        );
    }

    #[test]
    fn a_reorder_on_one_side_collides_with_an_edit_the_reorder_touches() {
        // Mine swaps the two tunings; theirs edits a field inside the first one. The swap
        // touches both indices, so it overlaps theirs' edit at index 0.
        fn tuning(name: &str, kind: &str, pitches: Value) -> Value {
            json!({"name": name, "kind": kind, "pitches": pitches})
        }
        let old = json!({"tunings": [
            tuning("A", "x", json!([0, 100])),
            tuning("B", "x", json!([0, 200]))
        ]});
        let mine = json!({"tunings": [
            tuning("B", "x", json!([0, 200])),
            tuning("A", "x", json!([0, 100]))
        ]});
        let theirs = json!({"tunings": [
            tuning("A", "y", json!([0, 100])),
            tuning("B", "x", json!([0, 200]))
        ]});
        assert_eq!(
            rebase(&old, &theirs, &mine, &APPEND),
            Rebased::Fields {
                content: theirs.clone(),
                conflicts: vec![kept("/tunings", mine["tunings"].clone())]
            }
        );
    }

    #[test]
    fn a_reorder_and_a_disjoint_retune_merge_by_position() {
        // Mine swaps indices 0 and 1; theirs retunes index 3. The changed-index sets ({0,1} vs
        // {3}) are disjoint, so rule (b) leaves them to merge by position: nothing is lost, but
        // "swap" is really just two replacements at stable positions, so the result carries
        // both edits rather than an actually-reordered list.
        let old = pitches(json!([0, 200, 400, 600]));
        let mine = pitches(json!([200, 0, 400, 600]));
        let theirs = pitches(json!([0, 200, 400, 650]));
        assert_eq!(
            rebase(&old, &theirs, &mine, &APPEND),
            Rebased::Clean(pitches(json!([200, 0, 400, 650])))
        );
    }

    #[test]
    fn both_sides_retuning_the_same_degree_collides_at_the_array() {
        let mine = pitches(json!([0, 200, 386, 500, 700, 900]));
        let theirs = pitches(json!([0, 200, 395, 500, 700, 900]));
        assert_eq!(
            rebase(&scale(), &theirs, &mine, &APPEND),
            list_kept_aside(theirs, mine)
        );
    }

    #[test]
    fn an_atomic_outer_list_overrides_an_inner_append_rule() {
        let lists = ListMergeConfig {
            default: ListMergePolicy::Append,
            rules: vec![(PathPattern("/tunings".into()), ListMergePolicy::Atomic)],
        };
        let mine = json!({"tunings": [
            {"name": "A", "pitches": [0, 101]},
            {"name": "B", "pitches": [0, 200]}
        ]});
        let theirs = json!({"tunings": [
            {"name": "A", "pitches": [0, 100]},
            {"name": "B", "pitches": [0, 201]}
        ]});
        assert_eq!(
            rebase(&two_tunings(), &theirs, &mine, &lists),
            Rebased::Fields {
                content: theirs.clone(),
                conflicts: vec![kept("/tunings", mine["tunings"].clone())]
            }
        );
    }

    #[test]
    fn an_append_on_an_empty_list_applies_but_both_sides_appending_conflicts() {
        let empty = pitches(json!([]));
        let mine = pitches(json!([100]));
        assert_eq!(
            rebase(&empty, &empty, &mine, &APPEND),
            Rebased::Clean(mine.clone())
        );
        let theirs = pitches(json!([200]));
        assert_eq!(
            rebase(&empty, &theirs, &mine, &APPEND),
            list_kept_aside(theirs, mine)
        );
    }

    #[test]
    fn both_sides_removing_the_same_element_from_a_root_list_is_clean() {
        let old = json!([0, 200, 400, 500]);
        let mine = json!([0, 400, 500]);
        let theirs = json!([0, 400, 500]);
        assert_eq!(
            rebase(&old, &theirs, &mine, &APPEND),
            Rebased::Clean(theirs)
        );
    }

    #[test]
    fn removing_one_twin_while_retuning_the_other_collides_at_the_array() {
        // With a repeated value, "which twin" is ambiguous: the disjoint index sets here would
        // otherwise merge, silently retargeting the removal at the wrong twin.
        let old = pitches(json!([0, 200, 200, 500]));
        let mine = pitches(json!([0, 200, 500, 800]));
        let theirs = pitches(json!([0, 204, 200, 500]));
        assert_eq!(
            rebase(&old, &theirs, &mine, &APPEND),
            list_kept_aside(theirs, mine)
        );
    }

    #[test]
    fn an_append_against_a_disjoint_retune_of_a_repeated_value_collides_at_the_array() {
        let old = pitches(json!([0, 200, 200, 500]));
        let mine = pitches(json!([0, 200, 200, 500, 800]));
        let theirs = pitches(json!([0, 200, 200, 501]));
        assert_eq!(
            rebase(&old, &theirs, &mine, &APPEND),
            list_kept_aside(theirs, mine)
        );
    }

    #[test]
    fn a_disjoint_retune_with_no_repeated_values_still_merges() {
        let mine = pitches(json!([0, 200, 386, 500, 700, 900]));
        let theirs = pitches(json!([0, 200, 400, 500, 700, 884]));
        assert_eq!(
            rebase(&scale(), &theirs, &mine, &APPEND),
            Rebased::Clean(pitches(json!([0, 200, 386, 500, 700, 884])))
        );
    }

    #[test]
    fn both_sides_removing_the_same_element_from_a_repeated_value_list_is_clean() {
        let old = pitches(json!([0, 200, 200, 500]));
        let mine = pitches(json!([0, 200, 500]));
        let theirs = pitches(json!([0, 200, 500]));
        assert_eq!(
            rebase(&old, &theirs, &mine, &APPEND),
            Rebased::Clean(theirs)
        );
    }
}

#[cfg(test)]
mod property_tests {
    //! One writable document against a model server that applies updates and deletes
    //! (deduplicated by `(upload_id, base_hash)`) and that another device edits or deletes.
    //! Both sides edit `pitches` with random inserts, removes, retunes, moves and appends; the
    //! client also appends to `items` and both write the scalar `shared`. Every seed runs under
    //! the `Append` and the `Atomic` list policy. Changes reach the client as pushes (envelope
    //! at the change's seq) or catch-up pages (current envelope). A lost reply is either a
    //! reconnect (every committed change is delivered before the next build) or silent (the
    //! client learns of it only through `hash_mismatch`). Like the machine, the client applies
    //! a server copy only once it has been delivered every change up to the copy's seq;
    //! otherwise it catches up and rebuilds. Elements are unique across the run, so the
    //! repeated-values list rule is covered by unit tests, not by this model.
    use std::collections::HashMap;

    use super::change_fixtures::env;
    use super::fixtures::*;
    use super::*;
    use crate::engine::backoff::Jitter;
    use crate::engine::doc_upload::reply_diverged;
    use crate::engine::types::{ServerError, Upload, UploadKind};
    use serde_json::json;

    const SEEDS: u64 = 500;
    const STEPS: usize = 80;
    const MAX_LOCAL_EDITS: u32 = 12;
    const MAX_OTHER_EDITS: u32 = 8;

    static ATOMIC: ListMergeConfig = ListMergeConfig {
        default: ListMergePolicy::Atomic,
        rules: Vec::new(),
    };

    #[derive(Debug, Clone, Copy)]
    struct Committed {
        seq: Seq,
        upload_id: Option<Uuid>,
        deleted: bool,
    }

    /// How often each rarely reached branch ran.
    #[derive(Debug, Default)]
    struct Hits {
        /// Rebase conflicts hit by a change-feed delivery (push or catch-up page).
        rebase_conflicts_delivery: u32,
        /// Rebase conflicts hit while settling a fetched server copy.
        rebase_conflicts_server_copy: u32,
        server_copy_fetches: u32,
        server_copy_waits: u32,
        /// Deletes with no shadow that fetched the server copy before going out.
        delete_copy_fetches: u32,
        mismatch_retries: u32,
        delete_wins: u32,
        local_deletes_settled: u32,
        equal_content_adopts: u32,
        never_sent_rebases: u32,
        field_conflicts: u32,
        /// Rebases where both sides changed `pitches` at the same length and it merged.
        list_merges: u32,
        /// Rebases where one side only appended to `pitches` and it merged.
        append_merges: u32,
        /// Rebases that kept the server's `pitches` and set the local list aside.
        list_conflicts: u32,
        /// Pending deletes a newer other-device version superseded.
        deletes_superseded: u32,
        /// Superseded deletes that kept the user's earlier edits as a copy.
        deletes_superseded_kept: u32,
    }

    impl Hits {
        fn add(&mut self, other: &Hits) {
            self.rebase_conflicts_delivery += other.rebase_conflicts_delivery;
            self.rebase_conflicts_server_copy += other.rebase_conflicts_server_copy;
            self.server_copy_fetches += other.server_copy_fetches;
            self.server_copy_waits += other.server_copy_waits;
            self.delete_copy_fetches += other.delete_copy_fetches;
            self.mismatch_retries += other.mismatch_retries;
            self.delete_wins += other.delete_wins;
            self.local_deletes_settled += other.local_deletes_settled;
            self.equal_content_adopts += other.equal_content_adopts;
            self.never_sent_rebases += other.never_sent_rebases;
            self.field_conflicts += other.field_conflicts;
            self.list_merges += other.list_merges;
            self.append_merges += other.append_merges;
            self.list_conflicts += other.list_conflicts;
            self.deletes_superseded += other.deletes_superseded;
            self.deletes_superseded_kept += other.deletes_superseded_kept;
        }
    }

    struct Server {
        /// Index = seq; seq 1 is the content both sides start from.
        history: Vec<Value>,
        changes: Vec<Committed>,
        stored: HashMap<(Uuid, Option<String>), Seq>,
        other_edits: u32,
        deleted: bool,
        /// (order, seq, value) of every other-device write to `shared`, in the order they
        /// happened; `None` value = removed. `order` comes from the shared counter every write
        /// to `shared` advances (see `Client::edit`), used to tell whether *some* other-device
        /// write happened after a given local one. `seq` is the server's real commit order,
        /// needed to tell whether this write's value actually won a race against a local upload
        /// that landed later in true time (see `check_shared_key_preserved`).
        other_shared_writes: Vec<(u64, Seq, Option<String>)>,
        /// (base hash named, hash of the content deleted over) of every client delete committed.
        deletes: Vec<(Option<String>, String)>,
    }

    impl Server {
        fn new(content: Value) -> Server {
            Server {
                history: vec![Value::Null, content],
                changes: Vec::new(),
                stored: HashMap::new(),
                other_edits: 0,
                deleted: false,
                other_shared_writes: Vec::new(),
                deletes: Vec::new(),
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

        fn commit(&mut self, content: Value, upload_id: Option<Uuid>, deleted: bool) -> Seq {
            self.history.push(content);
            let seq = self.seq();
            self.changes.push(Committed {
                seq,
                upload_id,
                deleted,
            });
            seq
        }

        /// Bumps `theirs` and sets or removes `shared`: setting or removing it collides with a
        /// concurrent local write (a field conflict). `order` is the counter every write to
        /// `shared`, from either side, advances (see `Client::edit`).
        fn other_device_edit(&mut self, rng: &mut Jitter, order: &mut u64) {
            if self.deleted || self.other_edits == MAX_OTHER_EDITS {
                return;
            }
            self.other_edits += 1;
            let mut next = self.current().clone();
            next["theirs"] = json!(self.other_edits);
            if rng.next_unit() < 0.5 {
                edit_pitches(&mut next, rng, "q", self.other_edits);
                if rng.next_unit() < 0.5 {
                    edit_pitches(&mut next, rng, "r", self.other_edits);
                }
            }
            let shared_write = if rng.next_unit() < 0.5 {
                let value = format!("theirs{}", self.other_edits);
                next["shared"] = json!(value);
                Some(Some(value))
            } else if next.get("shared").is_some() {
                // Removing a key that was never there changes nothing observable: not a write.
                next.as_object_mut()
                    .expect("content is an object")
                    .remove("shared");
                Some(None)
            } else {
                None
            };
            let seq = self.commit(next, None, false);
            if let Some(value) = shared_write {
                *order += 1;
                self.other_shared_writes.push((*order, seq, value));
            }
        }

        fn other_device_delete(&mut self) {
            if self.deleted {
                return;
            }
            self.deleted = true;
            self.commit(self.current().clone(), None, true);
        }

        fn mismatch(&self, current_hash: String) -> ServerError {
            let mut error = ServerError::new("hash_mismatch");
            error.current_hash = Some(current_hash);
            error.current_seq = Some(self.seq());
            error
        }

        fn upload(&mut self, upload: &Upload) -> Result<DocEnvelope, ServerError> {
            let key = (upload.upload_id, upload.base_hash.clone());
            if let Some(seq) = self.stored.get(&key) {
                return Ok(self.envelope(*seq));
            }
            if self.deleted {
                return Err(ServerError::new("not_found"));
            }
            let seq = match upload.kind {
                UploadKind::Delete => {
                    let current_hash = content_hash(self.current());
                    if upload
                        .base_hash
                        .as_ref()
                        .is_some_and(|base| *base != current_hash)
                    {
                        return Err(self.mismatch(current_hash));
                    }
                    self.deletes.push((upload.base_hash.clone(), current_hash));
                    self.deleted = true;
                    self.commit(self.current().clone(), Some(upload.upload_id), true)
                }
                UploadKind::Update => {
                    let current_hash = content_hash(self.current());
                    if upload.base_hash.as_deref() != Some(current_hash.as_str()) {
                        return Err(self.mismatch(current_hash));
                    }
                    let patch: json_patch::Patch =
                        serde_json::from_value(upload.payload.clone()).unwrap();
                    let mut next = self.current().clone();
                    json_patch::patch(&mut next, &patch).unwrap();
                    self.commit(next, Some(upload.upload_id), false)
                }
                UploadKind::Create => panic!("the model document always exists on the server"),
            };
            self.stored.insert(key, seq);
            Ok(self.envelope(seq))
        }
    }

    struct Client {
        snap: DocSnapshot,
        next_row: u128,
        in_flight: Option<(InFlight, Result<DocEnvelope, ServerError>)>,
        mismatches: u32,
        delivered: usize,
        /// Highest seq delivered: the model's committed `own` cursor.
        cursor: Seq,
        must_catch_up: bool,
        local_edits: u32,
        /// `n` of every `t{n}` appended to `items`.
        tokens: Vec<u32>,
        deleted_locally: bool,
        recovered: Vec<Value>,
        hits: Hits,
        /// List merge configuration passed to every apply and rebase call.
        lists: &'static ListMergeConfig,
        /// (order, value) of every local write to `shared`, in the order they happened —
        /// recorded in `edit`, the moment the write is made, whether or not it ever reaches an
        /// upload (a rebase can silently drop a pending edit before it is ever built). Used to
        /// tell whether a local write was ever accounted for at all.
        local_shared_writes: Vec<(u64, String)>,
        /// (seq, value) of the shared key's value in the last local upload that actually landed
        /// on the server and touched `/shared` — `seq` is the server's real commit order, which
        /// `local_shared_writes`'s `order` cannot give: a local write racing a concurrent
        /// other-device write is settled by whichever lands with a matching hash, not by which
        /// was queued first, so only a real landing seq can tell whether it won. The value is
        /// captured in `build`, at upload time, not from `snap.content` on the reply — content
        /// can have moved on by then, which would pair a stale seq with fresher content.
        landed_shared: Option<(Seq, String)>,
        /// Every field kept aside by a `RecoverFields`, across the whole run.
        field_recovered: Vec<FieldConflict>,
        /// (seq, value) of `shared` in every local upload that landed and touched it.
        landed_shared_writes: Vec<(Seq, String)>,
    }

    impl Client {
        fn new(content: Value, lists: &'static ListMergeConfig) -> Client {
            Client {
                snap: synced(content, 1),
                next_row: 0,
                in_flight: None,
                mismatches: 0,
                delivered: 0,
                cursor: 1,
                must_catch_up: false,
                local_edits: 0,
                tokens: Vec::new(),
                deleted_locally: false,
                recovered: Vec::new(),
                hits: Hits::default(),
                lists,
                local_shared_writes: Vec::new(),
                landed_shared: None,
                field_recovered: Vec::new(),
                landed_shared_writes: Vec::new(),
            }
        }

        /// A v2 create whose upload landed but whose reply was lost: no shadow yet, one pending
        /// `Create` row. Fuzzes the no-shadow branch of `apply_snapshot_doc` and of
        /// `apply_change`'s non-echo arm, plus its echo arm on the seeds where `server.changes`
        /// carries a matching entry.
        fn new_lost_create(content: Value, lists: &'static ListMergeConfig) -> Client {
            let mut c = Client::new(content, lists);
            c.snap.shadow = None;
            c.cursor = 0;
            c.next_row = 1;
            c.snap.rows = vec![OutboxRow {
                mutation_id: m(1),
                kind: RowKind::Create,
                parked: false,
            }];
            c.snap.unacked_upload = Some(m(1));
            c
        }

        fn count_pitch_merge(&mut self, merge: Option<PitchMerge>) {
            match merge {
                Some(PitchMerge::Positional) => self.hits.list_merges += 1,
                Some(PitchMerge::Appended) => self.hits.append_merges += 1,
                Some(PitchMerge::WholeList) => self.hits.list_conflicts += 1,
                None => {}
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
            let superseded = ops.contains(&DocOp::Emit(DocEvent::DeleteSuperseded));
            let edited_before_delete = superseded
                && self.snap.shadow.as_ref().map(|s| content_hash(&s.content))
                    != Some(content_hash(&self.snap.content));
            if superseded {
                let kept: Vec<&Value> = ops
                    .iter()
                    .filter_map(|op| match op {
                        DocOp::Recover {
                            content,
                            reason: RecoverReason::DeleteSuperseded,
                        } => Some(content),
                        _ => None,
                    })
                    .collect();
                if edited_before_delete {
                    assert_eq!(
                        kept,
                        vec![&self.snap.content],
                        "a superseded delete keeps the edits made before it"
                    );
                    self.hits.deletes_superseded_kept += 1;
                } else {
                    assert!(kept.is_empty(), "an unedited superseded delete kept a copy");
                }
            }
            for op in ops {
                match op {
                    DocOp::InsertMarker(kind) => self.push_row(*kind),
                    DocOp::Recover { content, reason } => {
                        if *reason == RecoverReason::DeleteWins {
                            self.hits.delete_wins += 1;
                        }
                        self.recovered.push(content.clone());
                    }
                    DocOp::RecoverFields { fields, .. } => {
                        self.field_recovered.extend(fields.iter().cloned());
                    }
                    DocOp::Emit(DocEvent::DeleteSuperseded) => self.hits.deletes_superseded += 1,
                    other => self.snap = self.snap.project(std::slice::from_ref(other)),
                }
            }
            // A hard delete without a tombstone (sweep, `not_found`) drops the shadow on purpose.
            if self.snap.exists {
                assert!(
                    self.snap.server_seq() >= seq_before,
                    "shadow seq went backwards"
                );
            }
        }

        /// A local edit as the store accepts it: never on a deleted document. `order` is the
        /// counter every write to `shared`, from either side, advances — assigned the moment
        /// the write is made, not the server seq it may or may not ever reach.
        fn edit(&mut self, rng: &mut Jitter, order: &mut u64) {
            if !self.snap.exists || self.snap.soft_deleted || self.local_edits == MAX_LOCAL_EDITS {
                return;
            }
            self.local_edits += 1;
            let n = self.local_edits;
            let roll = rng.next_unit();
            if roll < 0.3 {
                let value = format!("mine{n}");
                self.snap.content["shared"] = json!(value);
                *order += 1;
                self.local_shared_writes.push((*order, value));
            } else if roll < 0.6 {
                self.snap.content["items"]
                    .as_array_mut()
                    .expect("items array")
                    .push(token(n));
                self.tokens.push(n);
            } else {
                edit_pitches(&mut self.snap.content, rng, "p", n);
            }
            self.push_row(RowKind::Update);
        }

        /// A local delete as the store writes it: soft delete plus a `Delete` row.
        fn delete(&mut self) {
            if !self.snap.exists || self.snap.soft_deleted {
                return;
            }
            self.snap.soft_deleted = true;
            self.push_row(RowKind::Delete);
            self.deleted_locally = true;
        }

        /// A non-echo delivery or server copy onto a shadow with pending rows that recovers as
        /// a conflict is a rebase that failed.
        fn count_rebase_conflict(
            &mut self,
            rebase_possible: bool,
            ops: &[DocOp],
            via_server_copy: bool,
        ) {
            let whole_conflicted = ops.iter().any(|op| {
                matches!(
                    op,
                    DocOp::Recover {
                        reason: RecoverReason::Conflict,
                        ..
                    }
                )
            });
            let field_conflicted = ops
                .iter()
                .any(|op| matches!(op, DocOp::RecoverFields { .. }));
            if rebase_possible && (whole_conflicted || field_conflicted) {
                if via_server_copy {
                    self.hits.rebase_conflicts_server_copy += 1;
                } else {
                    self.hits.rebase_conflicts_delivery += 1;
                }
                if field_conflicted {
                    self.hits.field_conflicts += 1;
                }
            }
        }

        /// A pending document with no shadow whose content the envelope already holds (hashed
        /// locally on both sides) must be adopted, never recovered.
        fn equal_content_adoptable(&self, doc: &DocEnvelope, guard_seq: Seq) -> bool {
            self.snap.exists
                && self.snap.shadow.is_none()
                && !self.snap.rows.is_empty()
                && !delete_pending(&self.snap)
                && !doc.read_only
                && guard_seq > self.snap.server_seq()
                && content_hash(&self.snap.content) == content_hash(&doc.content)
        }

        fn check_adopted(&mut self, adoptable: bool, ops: &[DocOp]) {
            if !adoptable {
                return;
            }
            assert!(
                !ops.iter().any(|op| matches!(op, DocOp::Recover { .. })),
                "equal content must be adopted, not recovered"
            );
            assert!(
                self.snap.project(ops).rows.is_empty(),
                "adopting equal content settles every row"
            );
            self.hits.equal_content_adopts += 1;
        }

        fn deliver(&mut self, server: &Server, as_page: bool) {
            let Some(&committed) = server.changes.get(self.delivered) else {
                return;
            };
            self.delivered += 1;
            // Protocol.changes_since loads only the document's current row and drops any upsert
            // whose document is since deleted (its `live?/1` filter): the cursor still advances
            // past it, but a real catch-up page never delivers it at all.
            // A page read after the delete drops every upsert before it in one go: no later
            // push of those changes can follow that page.
            if as_page && server.deleted && !committed.deleted {
                while server
                    .changes
                    .get(self.delivered)
                    .is_some_and(|c| !c.deleted)
                {
                    self.delivered += 1;
                }
                self.cursor = self.cursor.max(server.changes[self.delivered - 1].seq);
                return;
            }
            let doc = (!committed.deleted).then(|| {
                if as_page {
                    server.envelope(server.seq())
                } else {
                    server.envelope(committed.seq)
                }
            });
            let change = Change {
                scope: "own".into(),
                seq: committed.seq,
                prev_seq: committed.seq - 1,
                doc_id: DOC,
                kind: if committed.deleted {
                    ChangeKind::Delete
                } else {
                    ChangeKind::Upsert
                },
                doc: doc.clone(),
                client_id: None,
                upload_id: committed.upload_id,
            };
            let is_echo_for_us = committed.upload_id.is_some_and(|u| self.rows_contain(u));
            let pending_delete_before = delete_pending(&self.snap);
            let rebase_possible = self.snap.shadow.is_some() && !self.snap.rows.is_empty();
            let before_server_seq = self.snap.server_seq();
            let old_shadow_content = self.snap.shadow.as_ref().map(|s| s.content.clone());
            let local_content_before = self.snap.content.clone();
            let adoptable = !is_echo_for_us
                && doc
                    .as_ref()
                    .is_some_and(|doc| self.equal_content_adoptable(doc, committed.seq));
            let ops = apply_change(&self.snap, &change, ME, self.lists);
            self.check_adopted(adoptable, &ops);
            self.count_rebase_conflict(rebase_possible && !is_echo_for_us, &ops, false);
            if rebase_possible
                && !is_echo_for_us
                && !pending_delete_before
                && committed.seq > before_server_seq
            {
                if let (Some(shadow_content), Some(doc)) = (&old_shadow_content, &doc) {
                    assert_shared_collision_kept_aside(
                        "deliver",
                        self,
                        shadow_content,
                        &local_content_before,
                        &doc.content,
                        &ops,
                    );
                    let merge = check_pitches(
                        "deliver",
                        self,
                        shadow_content,
                        &local_content_before,
                        &doc.content,
                        &ops,
                    );
                    self.count_pitch_merge(merge);
                }
            }
            self.apply(&ops);
            self.cursor = self.cursor.max(committed.seq);
            if let (false, Some(doc)) = (is_echo_for_us, &doc) {
                // apply_change's content guard gates on the change's own seq, not the
                // envelope's (a page can carry a newer envelope than the change it delivers).
                assert_other_device_edit_preserved(
                    before_server_seq,
                    committed.seq,
                    pending_delete_before,
                    doc,
                    self,
                    &ops,
                    "deliver",
                );
            }
        }

        fn rows_contain(&self, mutation_id: Uuid) -> bool {
            self.snap.rows.iter().any(|r| r.mutation_id == mutation_id)
        }

        fn catch_up(&mut self, server: &Server, rng: &mut Jitter) {
            while self.delivered < server.changes.len() {
                let as_page = rng.next_unit() < 0.5;
                self.deliver(server, as_page);
            }
            self.must_catch_up = false;
        }

        /// A full resync: the current envelope as a snapshot document, or the sweep when the
        /// server no longer lists the document; the cursor jumps past every committed change.
        fn snapshot(&mut self, server: &Server) {
            if server.deleted {
                let ops = sweep_doc(&self.snap, "own", server.seq());
                self.apply(&ops);
            } else {
                let doc = server.envelope(server.seq());
                // Rows rebase with a migrated v1 base or when none was sent unacknowledged; no
                // shadow at all is a conflict.
                let migrated_v1_base = self.snap.shadow.as_ref().is_some_and(|s| s.seq == 0);
                let adoptable = self.equal_content_adoptable(&doc, doc.seq);
                let recovers = !adoptable
                    && self.snap.exists
                    && !self.snap.rows.is_empty()
                    && !delete_pending(&self.snap)
                    && !doc.read_only
                    && !migrated_v1_base
                    && doc.seq > self.snap.server_seq()
                    && (self.snap.shadow.is_none() || self.snap.unacked_upload.is_some());
                // Rows never sent cannot be in the snapshot: a clean rebase must not recover them.
                let never_sent_rebase = self.snap.exists
                    && !self.snap.rows.is_empty()
                    && !delete_pending(&self.snap)
                    && !doc.read_only
                    && doc.seq > self.snap.server_seq()
                    && self.snap.unacked_upload.is_none()
                    && self.snap.shadow.as_ref().is_some_and(|shadow| {
                        matches!(
                            rebase(
                                &shadow.content,
                                &doc.content,
                                &self.snap.content,
                                self.lists
                            ),
                            Rebased::Clean(_) | Rebased::Fields { .. }
                        )
                    });
                let pre_content = self.snap.content.clone();
                let old_shadow_content = self.snap.shadow.as_ref().map(|s| s.content.clone());
                let ops = apply_snapshot_doc(&self.snap, "own", &doc, ME, self.lists);
                self.check_adopted(adoptable, &ops);
                if never_sent_rebase {
                    if let Some(shadow_content) = &old_shadow_content {
                        let merge = check_pitches(
                            "page",
                            self,
                            shadow_content,
                            &pre_content,
                            &doc.content,
                            &ops,
                        );
                        self.count_pitch_merge(merge);
                    }
                    assert!(
                        !ops.iter().any(|op| matches!(op, DocOp::Recover { .. })),
                        "never-sent edits were recovered instead of rebased"
                    );
                    self.hits.never_sent_rebases += 1;
                }
                if recovers {
                    assert!(
                        ops.contains(&DocOp::Recover {
                            content: pre_content,
                            reason: RecoverReason::Conflict,
                        }),
                        "a doc with pending rows and no migrated base must recover its exact local content"
                    );
                    assert!(
                        !ops.iter().any(|op| matches!(op, DocOp::InsertMarker(_))),
                        "a doc with pending rows and no migrated base must never rebase onto a snapshot"
                    );
                }
                self.apply(&ops);
            }
            self.delivered = server.changes.len();
            self.cursor = server.seq();
            self.must_catch_up = false;
        }

        fn build(&mut self, server: &mut Server, rng: &mut Jitter) {
            if self.in_flight.is_some() {
                return;
            }
            if self.must_catch_up {
                self.catch_up(server, rng);
            }
            let ends_in_delete = self
                .snap
                .rows
                .last()
                .is_some_and(|r| r.kind == RowKind::Delete);
            // The model server takes updates and deletes only: a pending create waits for a
            // page or snapshot to give it a shadow.
            if !self.snap.exists || (self.snap.shadow.is_none() && !ends_in_delete) {
                return;
            }
            match build_upload(&self.snap, ME) {
                BuildResult::Send { upload, inflight } => {
                    if upload.kind == UploadKind::Delete {
                        assert_eq!(
                            upload.base_hash,
                            self.snap.shadow.as_ref().map(|s| s.hash.clone()),
                            "a delete names the shadow's version whenever there is one"
                        );
                    }
                    self.snap.unacked_upload = self.snap.unacked_upload.max(Some(upload.upload_id));
                    // Captured now, not from `snap.content` on the reply: content can move on
                    // (a later local edit) before the reply for this exact upload comes back.
                    let shared_at_build = (upload.kind == UploadKind::Update
                        && upload_touches_shared(&upload.payload))
                    .then(|| self.snap.content.get("shared").and_then(Value::as_str))
                    .flatten()
                    .map(str::to_string);
                    let reply = server.upload(&upload);
                    if let (Some(value), Ok(envelope)) = (shared_at_build, &reply) {
                        self.landed_shared = Some((envelope.seq, value.clone()));
                        self.landed_shared_writes.push((envelope.seq, value));
                    }
                    self.in_flight = Some((inflight, reply));
                }
                // Like the machine's `forget_failures`: a document with nothing to send starts over.
                BuildResult::SettleLocally(ops) => {
                    self.mismatches = 0;
                    self.apply(&ops);
                }
                BuildResult::Nothing => self.mismatches = 0,
                BuildResult::NeedsServerCopy => {
                    assert!(
                        self.snap.shadow.is_none() && ends_in_delete,
                        "only a delete with no shadow asks for the server copy in the model"
                    );
                    self.hits.delete_copy_fetches += 1;
                    self.fetch_server_copy(server, rng);
                }
            }
        }

        fn fetch_server_copy(&mut self, server: &Server, rng: &mut Jitter) {
            self.hits.server_copy_fetches += 1;
            if server.deleted {
                let ops = apply_server_deleted(&self.snap, server.seq());
                self.apply(&ops);
                return;
            }
            let doc = server.envelope(server.seq());
            if doc.seq > self.cursor {
                // The copy may hold an upload of ours whose reply was lost;
                // catching up lets its echo settle our rows first, then we rebuild.
                self.hits.server_copy_waits += 1;
                self.catch_up(server, rng);
                return;
            }
            let pending_delete_before = delete_pending(&self.snap);
            let rebase_possible = self.snap.shadow.is_some() && !self.snap.rows.is_empty();
            let before_server_seq = self.snap.server_seq();
            let old_shadow_content = self.snap.shadow.as_ref().map(|s| s.content.clone());
            let local_content_before = self.snap.content.clone();
            let ops = apply_server_copy(&self.snap, &doc, ME, self.lists);
            self.count_rebase_conflict(rebase_possible, &ops, true);
            if rebase_possible && !pending_delete_before && doc.seq > before_server_seq {
                if let Some(shadow_content) = &old_shadow_content {
                    assert_shared_collision_kept_aside(
                        "server copy",
                        self,
                        shadow_content,
                        &local_content_before,
                        &doc.content,
                        &ops,
                    );
                    let merge = check_pitches(
                        "server copy",
                        self,
                        shadow_content,
                        &local_content_before,
                        &doc.content,
                        &ops,
                    );
                    self.count_pitch_merge(merge);
                }
            }
            self.apply(&ops);
            // apply_server_copy's content guard gates on the envelope's own seq.
            assert_other_device_edit_preserved(
                before_server_seq,
                doc.seq,
                pending_delete_before,
                &doc,
                self,
                &ops,
                "server copy",
            );
        }

        fn reply(&mut self, server: &Server, rng: &mut Jitter) {
            let Some((inflight, reply)) = self.in_flight.take() else {
                return;
            };
            assert!(
                !reply_diverged(&self.snap, &inflight, &reply),
                "the model server never transforms content, so a divergent reply is false"
            );
            match settle(&self.snap, &inflight, &reply, ME, self.mismatches, 0) {
                SettleResult::Ops(ops) => {
                    if inflight.kind == UploadKind::Delete {
                        self.hits.local_deletes_settled += 1;
                    }
                    self.mismatches = 0;
                    self.apply(&ops);
                }
                SettleResult::FetchServerCopy => {
                    self.mismatches += 1;
                    self.fetch_server_copy(server, rng);
                }
                SettleResult::Retry { mismatch, .. } => {
                    if mismatch {
                        self.hits.mismatch_retries += 1;
                        self.mismatches += 1;
                    }
                }
            }
        }

        /// The reply is lost with the connection: the next one catches up before building.
        fn lose_reply(&mut self) {
            if self.in_flight.take().is_some() {
                self.must_catch_up = true;
            }
        }

        /// The reply is lost but the connection stays: nothing is delivered before the rebuild.
        fn lose_reply_silently(&mut self) {
            self.in_flight = None;
        }
    }

    fn token(n: u32) -> Value {
        json!(format!("t{n}"))
    }

    /// One random retune (replace), remove, insert, move or append on `pitches`. A new element
    /// is `{side}{n}`, unique across the run; a move keeps its element.
    fn edit_pitches(content: &mut Value, rng: &mut Jitter, side: &str, n: u32) {
        let pitches = content["pitches"].as_array_mut().expect("pitches array");
        let element = json!(format!("{side}{n}"));
        let len = pitches.len();
        match (rng.next_unit() * 5.0) as u32 {
            0 if len > 0 => {
                let at = index_below(rng, len);
                pitches[at] = element;
            }
            1 if len > 0 => {
                let at = index_below(rng, len);
                pitches.remove(at);
            }
            2 => {
                let at = index_below(rng, len + 1);
                pitches.insert(at, element);
            }
            3 if len >= 2 => {
                let from = index_below(rng, len);
                let moved = pitches.remove(from);
                let to = index_below(rng, len);
                pitches.insert(to, moved);
            }
            _ => pitches.push(element),
        }
    }

    fn index_below(rng: &mut Jitter, len: usize) -> usize {
        ((rng.next_unit() * len as f64) as usize).min(len - 1)
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum PitchMerge {
        /// Both sides kept the length; merged position by position.
        Positional,
        /// One side only appended, the other kept the length.
        Appended,
        /// The server's list was kept and the local list set aside.
        WholeList,
    }

    fn pitches(content: &Value) -> Vec<Value> {
        content["pitches"].as_array().cloned().unwrap_or_default()
    }

    fn only_appends(old: &[Value], list: &[Value]) -> bool {
        list.len() > old.len() && list[..old.len()] == old[..]
    }

    /// At a rebase, `pitches` is either set aside whole (the server's list taken) or merged with
    /// nothing lost, duplicated or revived. The order of a pair is kept when at least one side
    /// holds both elements and no side holding both disagrees; a pair is skipped when either
    /// side holds exactly one of the two. Nothing comes from neither side. An atomic list both sides changed to
    /// different lists is always set aside whole.
    fn check_pitches(
        what: &str,
        client: &Client,
        old_shadow: &Value,
        local_before: &Value,
        incoming: &Value,
        ops: &[DocOp],
    ) -> Option<PitchMerge> {
        if ops.iter().any(|op| matches!(op, DocOp::Recover { .. })) {
            return None;
        }
        let old = pitches(old_shadow);
        let mine = pitches(local_before);
        let theirs = pitches(incoming);
        let result = pitches(&client.snap.project(ops).content);
        let fields: Vec<&FieldConflict> = ops
            .iter()
            .filter_map(|op| match op {
                DocOp::RecoverFields { fields, .. } => Some(fields),
                _ => None,
            })
            .flatten()
            .collect();
        let both_changed = mine != old && theirs != old && mine != theirs;
        if both_changed && client.lists.policy_for("/pitches") == ListMergePolicy::Atomic {
            assert!(
                fields.iter().any(|f| f.path == "/pitches"),
                "{what}: an atomic list both sides changed is set aside whole"
            );
        }
        if let Some(whole) = fields.iter().find(|f| f.path == "/pitches") {
            assert_eq!(
                result, theirs,
                "{what}: a list set aside takes the server's list"
            );
            assert_eq!(
                whole.local_value,
                Value::Array(mine.clone()),
                "{what}: the whole local list is kept aside"
            );
            return Some(PitchMerge::WholeList);
        }
        assert!(
            !fields.iter().any(|f| f.path.starts_with("/pitches/")),
            "{what}: a list conflict is whole, never per element"
        );
        for element in mine.iter().chain(&theirs) {
            let copies = result.iter().filter(|e| *e == element).count();
            assert!(copies <= 1, "{what}: {element} duplicated: {result:?}");
            let wanted =
                !old.contains(element) || (mine.contains(element) && theirs.contains(element));
            assert_eq!(
                copies,
                usize::from(wanted),
                "{what}: {element} lost or revived \
                 (old {old:?}, mine {mine:?}, theirs {theirs:?}, result {result:?})"
            );
        }
        for element in &result {
            assert!(
                mine.contains(element) || theirs.contains(element),
                "{what}: {element} came from neither side"
            );
        }
        for (at, first) in result.iter().enumerate() {
            for second in &result[at + 1..] {
                // Per side: None if it holds exactly one of the pair, else whether it holds both
                // (and, if so, whether `first` comes before `second`).
                let sides = [&mine, &theirs].map(|side| {
                    let first_at = side.iter().position(|e| e == first);
                    let second_at = side.iter().position(|e| e == second);
                    match (first_at, second_at) {
                        (Some(a), Some(b)) => Some(Some(a < b)),
                        (None, None) => Some(None),
                        _ => None,
                    }
                });
                let [Some(in_mine), Some(in_theirs)] = sides else {
                    continue;
                };
                let kept_before = match (in_mine, in_theirs) {
                    (Some(a), Some(b)) if a != b => continue,
                    (Some(a), _) | (_, Some(a)) => a,
                    (None, None) => continue,
                };
                assert!(
                    kept_before,
                    "{what}: {first} and {second} are out of order \
                     (mine {mine:?}, theirs {theirs:?}, result {result:?})"
                );
            }
        }
        if !both_changed {
            None
        } else if only_appends(&old, &mine) || only_appends(&old, &theirs) {
            Some(PitchMerge::Appended)
        } else {
            Some(PitchMerge::Positional)
        }
    }

    /// Whether a JSON Patch upload payload includes an operation on `/shared`.
    fn upload_touches_shared(payload: &Value) -> bool {
        payload.as_array().is_some_and(|ops| {
            ops.iter()
                .any(|op| op.get("path").and_then(Value::as_str) == Some("/shared"))
        })
    }

    /// The other-device edit counter this content reflects (0 before any `other_device_edit`).
    fn theirs_of(content: &Value) -> u64 {
        content.get("theirs").and_then(Value::as_u64).unwrap_or(0)
    }

    /// Asserts a non-echo delivery that actually applies (content guard passed, no pending
    /// delete in play) never silently drops the other-device edit the incoming envelope
    /// carries: the client's content must catch up to it, or the delivery must explicitly
    /// recover the pre-delivery local content instead (tracked in `client.recovered`, not
    /// discarded). A stale or pending-delete delivery is exempt: it is spec-correct for those to
    /// leave content untouched. `guard_seq` is whatever seq the caller's own content guard
    /// compares against `before_server_seq`.
    fn assert_other_device_edit_preserved(
        before_server_seq: Seq,
        guard_seq: Seq,
        pending_delete_before: bool,
        envelope: &DocEnvelope,
        client: &Client,
        ops: &[DocOp],
        what: &str,
    ) {
        if pending_delete_before || guard_seq <= before_server_seq {
            return;
        }
        let recovered_now = ops.iter().any(|op| matches!(op, DocOp::Recover { .. }));
        let after = theirs_of(&client.snap.content);
        let envelope_theirs = theirs_of(&envelope.content);
        assert!(
            after >= envelope_theirs || recovered_now,
            "{what}: an other-device edit (theirs={envelope_theirs}) was dropped without a \
             Recover (client theirs is now {after})"
        );
    }

    /// At a rebase where both sides changed `shared` from the old shadow to different values,
    /// the server's value must be the result and the local one kept aside: in a
    /// `field_conflict` entry naming `/shared`, or in a whole-document `Recover`.
    fn assert_shared_collision_kept_aside(
        what: &str,
        client: &Client,
        old_shadow: &Value,
        local_before: &Value,
        incoming: &Value,
        ops: &[DocOp],
    ) {
        let old = old_shadow.get("shared");
        let mine = local_before.get("shared");
        let theirs = incoming.get("shared");
        if mine == old || theirs == old || mine == theirs {
            return;
        }
        let whole = ops.iter().any(|op| matches!(op, DocOp::Recover { .. }));
        let field = ops.iter().any(|op| {
            matches!(op, DocOp::RecoverFields { fields, .. } if fields.iter().any(|f| {
                f.path == "/shared"
                    && f.local_removed == mine.is_none()
                    && (f.local_removed || Some(&f.local_value) == mine)
            }))
        });
        assert!(
            whole || field,
            "{what}: colliding shared (mine={mine:?} theirs={theirs:?}) was not kept aside"
        );
        if field {
            assert_eq!(
                client.snap.project(ops).content.get("shared"),
                theirs,
                "{what}: a colliding shared must take the server's value"
            );
        }
    }

    /// The `shared` key is a single slot both sides write, checked two ways with two different
    /// notions of "later" — call order for "was it ever accounted for", real commit `seq` for
    /// "did it actually win a race" — because they answer different questions and neither
    /// substitutes for the other:
    ///
    /// 1. Every intended local write must be the final server value, kept aside (a
    ///    `field_conflict` entry for `/shared` or a whole-document copy), superseded by a later
    ///    local write, or overwritten by another device *after* it landed (commit order).
    ///
    /// 2. The latest other-device write must be the final content unless a *local write that
    ///    actually landed* (`landed_shared`, only set once a build's upload lands, with the
    ///    server's real `seq`) did so later in true commit order. This must use `seq`, not call
    ///    order: two genuinely concurrent writes — neither side aware of the other yet — are
    ///    settled by whichever upload's hash matches the server first, not by which was queued
    ///    first (seed 73: a local edit queued *before* a same-key other-device write can still
    ///    legitimately win, because its base hash still matched when it was finally built and
    ///    sent — a hash-based race, not a silent revert). Using call order here produced false
    ///    positives on exactly this kind of correct, concurrent resolution.
    ///
    /// Skipped when a local delete intentionally discarded the row.
    fn check_shared_key_preserved(seed: u64, client: &Client, server: &Server) {
        let final_shared = server.current().get("shared").and_then(Value::as_str);
        for (order, value) in &client.local_shared_writes {
            let superseded_by_later_local = client
                .local_shared_writes
                .iter()
                .any(|(local_order, _)| local_order > order);
            if superseded_by_later_local {
                continue;
            }
            let overwritten_after_landing = client
                .landed_shared_writes
                .iter()
                .filter(|(_, landed)| landed == value)
                .any(|(landed_seq, _)| {
                    server
                        .other_shared_writes
                        .iter()
                        .any(|(_, other_seq, _)| other_seq > landed_seq)
                });
            let field_kept = client
                .field_recovered
                .iter()
                .any(|f| f.path == "/shared" && !f.local_removed && f.local_value == json!(value));
            let whole_kept = client
                .recovered
                .iter()
                .any(|c| c.get("shared").and_then(Value::as_str) == Some(value.as_str()));
            assert!(
                final_shared == Some(value.as_str())
                    || field_kept
                    || whole_kept
                    || overwritten_after_landing,
                "seed {seed}: local shared {value:?} (order {order}) neither final nor kept \
                 aside (final={final_shared:?})"
            );
        }
        if let Some((_, other_seq, other_value)) = server
            .other_shared_writes
            .iter()
            .max_by_key(|(order, _, _)| *order)
        {
            let superseded = client
                .landed_shared
                .as_ref()
                .is_some_and(|(landed_seq, _)| landed_seq > other_seq);
            if !superseded {
                assert_eq!(
                    final_shared,
                    other_value.as_deref(),
                    "seed {seed}: other-device shared {other_value:?} (seq {other_seq}) \
                     replaced by a stale local value without a Recover"
                );
            }
        }
    }

    fn check_invariants(seed: u64, client: &Client) {
        if !client.snap.exists {
            assert!(
                client.snap.rows.is_empty(),
                "seed {seed}: rows left on a hard-deleted document"
            );
            return;
        }
        let items = client.snap.content["items"]
            .as_array()
            .expect("items array");
        for &n in &client.tokens {
            let copies = items.iter().filter(|item| **item == token(n)).count();
            assert!(
                copies <= 1,
                "seed {seed}: t{n} applied {copies} times: {items:?}"
            );
        }
        let list = pitches(&client.snap.content);
        for (at, element) in list.iter().enumerate() {
            assert!(
                !list[at + 1..].contains(element),
                "seed {seed}: {element} twice in pitches: {list:?}"
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
            client.reply(server, rng);
            client.catch_up(server, rng);
            check_invariants(seed, client);
            if client.snap.rows.is_empty() {
                return;
            }
            let ends_in_delete = client
                .snap
                .rows
                .last()
                .is_some_and(|r| r.kind == RowKind::Delete);
            if client.snap.shadow.is_none() && !ends_in_delete {
                // A pending create no delivered change resolved: a later connection's resync.
                client.snapshot(server);
            }
            client.build(server, rng);
        }
        panic!("seed {seed}: rows never settled: {:?}", client.snap.rows);
    }

    /// Runs every seed with `lists`; returns the hits after asserting the ones every policy
    /// must reach.
    fn run_model(lists: &'static ListMergeConfig) -> Hits {
        let mut totals = Hits::default();
        for seed in 1..=SEEDS {
            let mut rng = Jitter::new(seed * 104_729);
            let start = json!({
                "items": [],
                "theirs": 0,
                "pitches": ["b0", "b1", "b2", "b3", "b4", "b5"]
            });
            let mut server = Server::new(start.clone());
            // Half the seeds start as a lost-create client (no shadow, one pending Create row).
            let lost_create = seed % 2 == 0;
            let mut client = if lost_create {
                Client::new_lost_create(start, lists)
            } else {
                Client::new(start, lists)
            };
            if lost_create && seed % 4 == 0 {
                // The create landed as the doc's starting content but our reply was lost;
                // deliver it back as a change too, fuzzing the echo arm with no shadow.
                server.changes.push(Committed {
                    seq: 1,
                    upload_id: Some(m(1)),
                    deleted: false,
                });
            }
            // A delete ends the document's life, so only some seeds allow one.
            let local_deletes = seed % 5 == 0;
            // The model server takes no creates, so a lost create can never settle once the
            // document is deleted server-side.
            let other_deletes = !lost_create && seed % 7 == 0;
            if lost_create && seed % 3 == 0 {
                client.snapshot(&server);
            }
            // Advanced only by a write to `shared`, from either side, the moment it happens —
            // not the server seq (a dropped local write never reaches one) and not the step
            // index (most steps never touch `shared`).
            let mut shared_order: u64 = 0;
            for _ in 0..STEPS {
                match (rng.next_unit() * 11.0) as u32 {
                    0 => client.edit(&mut rng, &mut shared_order),
                    1 => client.build(&mut server, &mut rng),
                    2 => server.other_device_edit(&mut rng, &mut shared_order),
                    3 => client.deliver(&server, false),
                    4 => client.deliver(&server, true),
                    5 => client.reply(&server, &mut rng),
                    6 => client.lose_reply(),
                    7 => client.lose_reply_silently(),
                    8 if local_deletes => client.delete(),
                    9 if other_deletes => server.other_device_delete(),
                    8 | 9 => client.edit(&mut rng, &mut shared_order),
                    _ => client.snapshot(&server),
                }
                check_invariants(seed, &client);
            }
            drain(seed, &mut server, &mut client, &mut rng);
            for (base, deleted_over) in &server.deletes {
                assert!(
                    base.as_ref() == Some(deleted_over),
                    "seed {seed}: a delete destroyed a version the client had not seen"
                );
            }

            if server.deleted {
                assert!(
                    !client.snap.exists,
                    "seed {seed}: the server deleted the document but the client kept it"
                );
            } else {
                assert_eq!(
                    &client.snap.content,
                    server.current(),
                    "seed {seed}: client and server differ"
                );
            }
            // A local delete intentionally discards local edits.
            if !client.deleted_locally {
                let final_items = server.current()["items"].as_array().unwrap();
                for &n in &client.tokens {
                    let kept = final_items.contains(&token(n))
                        || client.recovered.iter().any(|content| {
                            content["items"]
                                .as_array()
                                .is_some_and(|items| items.contains(&token(n)))
                        });
                    assert!(kept, "seed {seed}: t{n} lost");
                }
                check_shared_key_preserved(seed, &client, &server);
            }
            // A lost create with no later edit on either side is identical to what landed.
            if lost_create
                && client.local_edits == 0
                && server.other_edits == 0
                && !server.deleted
                && !client.deleted_locally
            {
                assert!(
                    client.recovered.is_empty(),
                    "seed {seed}: a lost create with no later edit was recovered"
                );
            }
            totals.add(&client.hits);
        }
        // Each branch must actually run, or the model has stopped testing it.
        assert!(totals.rebase_conflicts_delivery > 0, "{totals:?}");
        assert!(totals.server_copy_fetches > 0, "{totals:?}");
        assert!(totals.server_copy_waits > 0, "{totals:?}");
        assert!(totals.delete_copy_fetches > 0, "{totals:?}");
        assert!(totals.mismatch_retries > 0, "{totals:?}");
        assert!(totals.delete_wins > 0, "{totals:?}");
        assert!(totals.local_deletes_settled > 0, "{totals:?}");
        assert!(totals.equal_content_adopts > 0, "{totals:?}");
        assert!(totals.never_sent_rebases > 0, "{totals:?}");
        assert!(totals.field_conflicts > 0, "{totals:?}");
        assert!(totals.list_conflicts > 0, "{totals:?}");
        assert!(totals.deletes_superseded > 0, "{totals:?}");
        totals
    }

    #[test]
    fn random_echo_rebase_and_page_sequences_lose_and_duplicate_nothing() {
        let totals = run_model(&APPEND);
        assert!(totals.list_merges > 0, "{totals:?}");
        assert!(totals.append_merges > 0, "{totals:?}");
    }

    #[test]
    fn random_sequences_with_atomic_lists_lose_and_duplicate_nothing() {
        let totals = run_model(&ATOMIC);
        assert_eq!(
            (totals.list_merges, totals.append_merges),
            (0, 0),
            "an atomic list both sides changed never merges: {totals:?}"
        );
    }
}

#[cfg(test)]
mod sweep_tests {
    use super::fixtures::*;
    use super::*;
    use serde_json::json;

    const CURATED: &str = "collection:curated";

    fn curated_doc(membership_seq: Seq) -> DocSnapshot {
        let mut s = synced(json!({"v": 1}), 3);
        s.owner_id = None;
        s.memberships = vec![Membership {
            scope: CURATED.into(),
            member: true,
            seq: membership_seq,
        }];
        s
    }

    fn left(seq: Seq) -> DocOp {
        DocOp::SetMembership(Membership {
            scope: CURATED.into(),
            member: false,
            seq,
        })
    }

    #[test]
    fn sweep_leaves_the_scope_and_deletes_an_orphan() {
        let ops = sweep_doc(&curated_doc(3), CURATED, 30);
        assert_eq!(ops, vec![left(30), DocOp::HardDelete]);
    }

    #[test]
    fn sweep_never_touches_docs_with_rows() {
        let mut s = curated_doc(3);
        s.rows = vec![row(1, RowKind::Update)];
        assert!(sweep_doc(&s, CURATED, 30).is_empty());
    }

    #[test]
    fn sweep_keeps_membership_newer_than_the_snapshot() {
        assert!(sweep_doc(&curated_doc(40), CURATED, 30).is_empty());
    }

    #[test]
    fn sweep_keeps_a_doc_that_is_a_member_elsewhere() {
        let mut s = curated_doc(3);
        s.memberships.push(Membership {
            scope: "own".into(),
            member: true,
            seq: 4,
        });
        assert_eq!(sweep_doc(&s, CURATED, 30), vec![left(30)]);
    }
}

#[cfg(test)]
mod snapshot_doc_tests {
    use super::change_fixtures::env;
    use super::fixtures::*;
    use super::*;
    use serde_json::json;

    /// Local copy at seq 1 with one pending append that was sent and never acknowledged; the
    /// snapshot (seq 5) already contains it, as it does when the reply was lost.
    fn pending_append() -> (DocSnapshot, DocEnvelope) {
        let mut s = synced(json!({"items": ["a"]}), 1);
        s.content = json!({"items": ["a", "b"]});
        s.rows = vec![row(1, RowKind::Update)];
        s.unacked_upload = Some(m(1));
        (s, env(json!({"items": ["a", "b"], "theirs": 1}), 5))
    }

    #[test]
    fn snapshot_doc_with_pending_rows_recovers_exact_local_content() {
        let (s, doc) = pending_append();
        let ops = apply_snapshot_doc(&s, "own", &doc, ME, &APPEND);
        assert!(ops.contains(&DocOp::Recover {
            content: json!({"items": ["a", "b"]}),
            reason: RecoverReason::Conflict,
        }));
        assert!(ops.contains(&DocOp::Emit(DocEvent::ConflictDetected)));
        assert!(!ops.iter().any(|op| matches!(op, DocOp::InsertMarker(_))));
        let after = s.project(&ops);
        assert_eq!(after.content, json!({"items": ["a", "b"], "theirs": 1}));
        assert!(after.rows.is_empty());
        assert_eq!(after.shadow.unwrap().seq, 5);
    }

    #[test]
    fn snapshot_doc_rebases_never_sent_pending_edits() {
        // A returning user: an edit made offline and never sent; another device changed the doc.
        let mut s = synced(json!({"items": ["a"]}), 1);
        s.content = json!({"items": ["a", "b"]});
        s.rows = vec![row(1, RowKind::Update)];
        let doc = env(json!({"items": ["a"], "theirs": 1}), 5);
        let ops = apply_snapshot_doc(&s, "own", &doc, ME, &APPEND);
        assert!(!ops
            .iter()
            .any(|op| matches!(op, DocOp::Recover { .. } | DocOp::Emit(_))));
        let after = s.project(&ops);
        assert_eq!(after.content, json!({"items": ["a", "b"], "theirs": 1}));
        assert_eq!(
            after.rows.len(),
            1,
            "the edit is still pending, now on the new base"
        );
        assert_eq!(after.shadow.unwrap().seq, 5);
    }

    #[test]
    fn settling_the_sent_rows_forgets_the_unacked_upload() {
        let mut s = synced(json!({}), 1);
        s.rows = vec![row(1, RowKind::Update), row(2, RowKind::Update)];
        s.unacked_upload = Some(m(1));
        assert_eq!(
            s.project(&[DocOp::DeleteRows(vec![m(2)])]).unacked_upload,
            Some(m(1)),
            "the sent row is still pending"
        );
        assert_eq!(
            s.project(&[DocOp::DeleteRows(vec![m(1)])]).unacked_upload,
            None
        );
        assert_eq!(s.project(&[DocOp::DropAllRows]).unacked_upload, None);
    }

    #[test]
    fn snapshot_doc_without_pending_rows_is_adopted_as_is() {
        let s = synced(json!({"items": ["a"]}), 1);
        let doc = env(json!({"items": ["x"]}), 5);
        let ops = apply_snapshot_doc(&s, "own", &doc, ME, &APPEND);
        assert!(!ops
            .iter()
            .any(|op| matches!(op, DocOp::Recover { .. } | DocOp::Emit(_))));
        let after = s.project(&ops);
        assert_eq!(after.content, json!({"items": ["x"]}));
        assert_eq!(after.shadow.unwrap().seq, 5);
    }

    #[test]
    fn snapshot_doc_that_changed_a_pending_delete_supersedes_it() {
        let (mut s, doc) = pending_append();
        s.soft_deleted = true;
        s.rows.push(row(2, RowKind::Delete));
        let ops = apply_snapshot_doc(&s, "own", &doc, ME, &APPEND);
        assert!(ops.contains(&DocOp::Emit(DocEvent::DeleteSuperseded)));
        assert!(
            ops.contains(&DocOp::Recover {
                content: json!({"items": ["a", "b"]}),
                reason: RecoverReason::DeleteSuperseded
            }),
            "the local edit made before the delete is kept"
        );
        let after = s.project(&ops);
        assert!(after.rows.is_empty() && !after.soft_deleted);
        assert_eq!(after.content, json!({"items": ["a", "b"], "theirs": 1}));
        assert_eq!(after.shadow.unwrap().seq, 5);
    }

    #[test]
    fn snapshot_doc_rebases_migrated_pending_edits() {
        // Migrated from v1: shadow from base_content at server_seq 0, one pending append.
        let mut s = synced(json!({"items": ["a"]}), 0);
        s.content = json!({"items": ["a", "b"]});
        s.rows = vec![row(1, RowKind::Update)];
        let doc = env(json!({"items": ["a"], "theirs": 1}), 5);
        let ops = apply_snapshot_doc(&s, "own", &doc, ME, &APPEND);
        assert!(!ops
            .iter()
            .any(|op| matches!(op, DocOp::Recover { .. } | DocOp::Emit(_))));
        let after = s.project(&ops);
        assert_eq!(after.content, json!({"items": ["a", "b"], "theirs": 1}));
        assert_eq!(after.rows.len(), 1, "the edit is still pending");
        assert_eq!(after.shadow.unwrap().seq, 5);
    }

    #[test]
    fn snapshot_doc_with_no_shadow_and_pending_create_recovers_exact_local_content() {
        // v2 local create: create upload landed, its reply was lost (no shadow yet). A snapshot
        // arrives holding both our create and another device's edit; this must never rebase.
        let mut s = synced(json!({"items": ["a"]}), 0);
        s.shadow = None;
        s.rows = vec![row(1, RowKind::Create)];
        let doc = env(json!({"items": ["a", "x"]}), 4);
        let ops = apply_snapshot_doc(&s, "own", &doc, ME, &APPEND);
        assert!(ops.contains(&DocOp::Recover {
            content: json!({"items": ["a"]}),
            reason: RecoverReason::Conflict,
        }));
        assert!(ops.contains(&DocOp::Emit(DocEvent::ConflictDetected)));
        assert!(!ops.iter().any(|op| matches!(op, DocOp::InsertMarker(_))));
        let after = s.project(&ops);
        assert_eq!(after.content, json!({"items": ["a", "x"]}));
        assert!(after.rows.is_empty());
        assert_eq!(after.shadow.unwrap().seq, 4);
    }

    #[test]
    fn snapshot_doc_is_skipped_by_the_content_guard() {
        let (s, _) = pending_append();
        let older = env(json!({"items": []}), 1);
        let ops = apply_snapshot_doc(&s, "own", &older, ME, &APPEND);
        assert!(!ops.iter().any(|op| matches!(
            op,
            DocOp::SetShadow(_) | DocOp::SetContent(_) | DocOp::Recover { .. }
        )));
    }
}

#[cfg(test)]
mod equal_content_tests {
    use super::change_fixtures::*;
    use super::fixtures::*;
    use super::*;
    use serde_json::json;

    /// A create that landed but whose reply was lost: no shadow, pending rows.
    fn lost_create(content: Value, rows: Vec<OutboxRow>) -> DocSnapshot {
        let mut s = synced(content, 0);
        s.shadow = None;
        s.rows = rows;
        s
    }

    fn assert_adopted(s: &DocSnapshot, ops: &[DocOp], seq: Seq) {
        assert!(!ops.iter().any(|op| matches!(
            op,
            DocOp::Recover { .. } | DocOp::Emit(DocEvent::ConflictDetected)
        )));
        let after = s.project(ops);
        assert_eq!(after.content, s.content, "local content is unchanged");
        assert!(
            after.rows.is_empty(),
            "every row is already in the envelope"
        );
        assert_eq!(after.shadow.expect("adopted shadow").seq, seq);
    }

    #[test]
    fn no_shadow_change_with_equal_content_adopts_and_settles() {
        let s = lost_create(json!({"items": ["a"]}), vec![row(1, RowKind::Create)]);
        let mut change = upsert("own", json!({"items": ["a"]}), 4);
        change.doc.as_mut().unwrap().hash = "server-hash".into();
        let ops = apply_change(&s, &change, ME, &APPEND);
        assert_adopted(&s, &ops, 4);
        assert_eq!(
            s.project(&ops).shadow.unwrap().hash,
            "server-hash",
            "the server hash is stored verbatim, never compared"
        );
    }

    #[test]
    fn no_shadow_snapshot_with_equal_content_adopts_and_settles() {
        let s = lost_create(
            json!({"items": ["a"]}),
            vec![row(1, RowKind::Create), row(2, RowKind::Update)],
        );
        let ops = apply_snapshot_doc(&s, "own", &env(json!({"items": ["a"]}), 4), ME, &APPEND);
        assert_adopted(&s, &ops, 4);
        assert!(!ops.iter().any(|op| matches!(op, DocOp::InsertMarker(_))));
    }

    #[test]
    fn no_shadow_server_copy_with_equal_content_adopts_and_settles() {
        let s = lost_create(json!({"n": 1}), vec![row(1, RowKind::Create)]);
        let ops = apply_server_copy(&s, &env(json!({"n": 1}), 2), ME, &APPEND);
        assert_adopted(&s, &ops, 2);
    }

    #[test]
    fn no_shadow_with_a_later_edit_still_recovers() {
        // The create landed as ["a"]; a later local edit made it ["a", "b"]: not equal.
        let s = lost_create(
            json!({"items": ["a", "b"]}),
            vec![row(1, RowKind::Create), row(2, RowKind::Update)],
        );
        let ops = apply_change(&s, &upsert("own", json!({"items": ["a"]}), 4), ME, &APPEND);
        assert!(ops.contains(&DocOp::Recover {
            content: json!({"items": ["a", "b"]}),
            reason: RecoverReason::Conflict,
        }));
        assert!(ops.contains(&DocOp::Emit(DocEvent::ConflictDetected)));
    }
}
