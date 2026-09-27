use serde_json::Value;
use uuid::Uuid;

use super::hash::content_hash;
use super::types::{Scope, Seq};

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
