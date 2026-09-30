//! Engine events become host events, appended to every attached handle's queue under one lock;
//! the same lock seeds a handle that attaches mid-connection.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tracing::warn;
use uuid::Uuid;

use super::lock;
use crate::driver::engine::EngineEvent;
use crate::engine::doc::DocEvent;
use crate::engine::machine::Lifecycle;
use crate::store::change_log::{ChangeOrigin, DocChange};
use crate::store::{DocNotice, Store, StoredDocument};

/// Who made a document change: a handle of this engine, sync, or another process's host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Local,
    Server,
    OtherProcess,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HostEvent {
    /// `attempt` counts dials since the last successful connection.
    ConnectionAttempted {
        attempt: u32,
    },
    ConnectionSucceeded,
    ConnectionLost,
    SyncStarted,
    SyncCompleted,
    SyncError {
        code: String,
        doc_id: Option<Uuid>,
        scope: Option<String>,
        fatal: bool,
        /// Local content kept aside with this error (`became_publication`, `create_rejected`,
        /// `delete_publication`, a refused delete's `delete_refused`).
        recovered_id: Option<i64>,
    },
    DocumentChanged {
        document: StoredDocument,
        origin: Origin,
    },
    DocumentDeleted {
        doc_id: Uuid,
        origin: Origin,
    },
    /// Local content was kept aside; `paths` is set for a field conflict.
    Conflict {
        doc_id: Uuid,
        reason: String,
        recovered_id: Option<i64>,
        paths: Vec<String>,
    },
    /// Changes were trimmed before this engine read them, or this handle fell behind: reload
    /// every list.
    DatabaseChanged,
    /// The data dir adopted the account's user id and restamped every owned document.
    IdentityAdopted {
        user_id: Uuid,
    },
}

/// Events one handle may hold before its document events collapse into one `DatabaseChanged`.
pub(super) const QUEUE_CAP: usize = 4096;

#[derive(Default)]
struct Queued {
    events: VecDeque<HostEvent>,
    /// Document events were collapsed; later ones are covered by the queued `DatabaseChanged`.
    lagged: bool,
}

#[derive(Default)]
pub(super) struct EventQueue(Mutex<Queued>);

fn is_document_event(event: &HostEvent) -> bool {
    matches!(
        event,
        HostEvent::DocumentChanged { .. }
            | HostEvent::DocumentDeleted { .. }
            | HostEvent::DatabaseChanged
    )
}

impl EventQueue {
    pub(super) fn push(&self, event: HostEvent) {
        let mut queued = lock(&self.0);
        let document = is_document_event(&event);
        if document && queued.lagged {
            return;
        }
        if matches!(event, HostEvent::ConnectionAttempted { .. }) {
            // Only the latest dial matters; a long offline spell must not push other events out.
            if let Some(attempted) =
                queued.events.iter_mut().rev().find(|queued_event| {
                    matches!(queued_event, HostEvent::ConnectionAttempted { .. })
                })
            {
                *attempted = event;
                return;
            }
        }
        if queued.events.len() >= QUEUE_CAP {
            if !queued.lagged {
                queued
                    .events
                    .retain(|queued_event| !is_document_event(queued_event));
                queued.events.push_back(HostEvent::DatabaseChanged);
                queued.lagged = true;
            }
            if document {
                return;
            }
            if queued.events.len() >= QUEUE_CAP {
                queued.events.pop_front();
            }
        }
        queued.events.push_back(event);
    }

    pub(super) fn take(&self) -> Vec<HostEvent> {
        let mut queued = lock(&self.0);
        queued.lagged = false;
        queued.events.drain(..).collect()
    }
}

/// What every attached handle has been told about the current connection.
#[derive(Default)]
struct Seed {
    connected: bool,
    started: bool,
    completed: bool,
    attempts: u32,
}

#[derive(Default)]
pub(super) struct FanOut {
    queues: Vec<Arc<EventQueue>>,
    seed: Seed,
    #[cfg(test)]
    pub(super) published: usize,
}

impl FanOut {
    /// A new handle's queue, starting from this connection's events so far (spec §3).
    pub(super) fn attach(&mut self) -> Arc<EventQueue> {
        let queue = Arc::new(EventQueue::default());
        if self.seed.connected {
            queue.push(HostEvent::ConnectionSucceeded);
            if self.seed.started {
                queue.push(HostEvent::SyncStarted);
            }
            if self.seed.completed {
                queue.push(HostEvent::SyncCompleted);
            }
        }
        self.queues.push(queue.clone());
        queue
    }

    pub(super) fn detach(&mut self, queue: &Arc<EventQueue>) {
        self.queues.retain(|attached| !Arc::ptr_eq(attached, queue));
    }

    fn publish(&mut self, event: HostEvent) {
        match &event {
            HostEvent::ConnectionSucceeded => {
                self.seed = Seed {
                    connected: true,
                    ..Seed::default()
                }
            }
            HostEvent::SyncStarted => self.seed.started = true,
            HostEvent::SyncCompleted => self.seed.completed = true,
            HostEvent::ConnectionLost => {
                self.seed = Seed {
                    attempts: self.seed.attempts,
                    ..Seed::default()
                }
            }
            _ => {}
        }
        #[cfg(test)]
        {
            self.published += 1;
        }
        for queue in &self.queues {
            queue.push(event.clone());
        }
    }

    fn publish_lifecycle(&mut self, lifecycle: Lifecycle) {
        let event = match lifecycle {
            Lifecycle::ConnectionAttempted => {
                self.seed.attempts += 1;
                HostEvent::ConnectionAttempted {
                    attempt: self.seed.attempts,
                }
            }
            Lifecycle::ConnectionSucceeded => HostEvent::ConnectionSucceeded,
            Lifecycle::ConnectionLost => HostEvent::ConnectionLost,
            Lifecycle::SyncStarted => HostEvent::SyncStarted,
            Lifecycle::SyncCompleted => HostEvent::SyncCompleted,
            Lifecycle::SyncError {
                code,
                scope,
                doc_id,
                fatal,
            } => HostEvent::SyncError {
                code,
                doc_id,
                scope,
                fatal,
                recovered_id: None,
            },
        };
        self.publish(event);
    }
}

/// Ends when the engine's owner stops (its event sender drops).
pub(super) async fn run(
    store: Arc<Store>,
    mut events: mpsc::UnboundedReceiver<EngineEvent>,
    fanout: Arc<Mutex<FanOut>>,
) {
    while let Some(event) = events.recv().await {
        let host_event = match event {
            EngineEvent::Lifecycle(lifecycle) => {
                lock(&fanout).publish_lifecycle(lifecycle);
                continue;
            }
            EngineEvent::Changed(change) => changed(&store, change).await,
            EngineEvent::Doc(notice) => noticed(notice),
            EngineEvent::DatabaseChanged => HostEvent::DatabaseChanged,
            EngineEvent::IdentityAdopted { user_id } => HostEvent::IdentityAdopted { user_id },
        };
        lock(&fanout).publish(host_event);
    }
}

async fn changed(store: &Store, change: DocChange) -> HostEvent {
    let origin = match change.origin {
        ChangeOrigin::Local => Origin::Local,
        ChangeOrigin::OtherProcess => Origin::OtherProcess,
        ChangeOrigin::Server => Origin::Server,
    };
    if change.deleted {
        return HostEvent::DocumentDeleted {
            doc_id: change.doc_id,
            origin,
        };
    }
    match store.get_document(change.doc_id).await {
        Ok(Some(document)) => HostEvent::DocumentChanged { document, origin },
        // Deleted again before this read.
        Ok(None) => HostEvent::DocumentDeleted {
            doc_id: change.doc_id,
            origin,
        },
        Err(error) => {
            warn!(%error, doc_id = %change.doc_id, "could not read a changed document; asking the host to reload");
            HostEvent::DatabaseChanged
        }
    }
}

fn noticed(notice: DocNotice) -> HostEvent {
    let recovered_id = notice.kept.as_ref().map(|kept| kept.recovered_id);
    match notice.event {
        DocEvent::ConflictDetected => HostEvent::Conflict {
            doc_id: notice.doc_id,
            reason: notice
                .kept
                .map_or_else(|| "conflict".to_string(), |kept| kept.reason),
            recovered_id,
            paths: Vec::new(),
        },
        DocEvent::FieldConflict { paths } => HostEvent::Conflict {
            doc_id: notice.doc_id,
            reason: "field_conflict".to_string(),
            recovered_id,
            paths,
        },
        // Unedited, nothing was kept: still reported, since the document the user deleted is back.
        DocEvent::DeleteSuperseded => HostEvent::Conflict {
            doc_id: notice.doc_id,
            reason: "delete_superseded".to_string(),
            recovered_id,
            paths: Vec::new(),
        },
        DocEvent::SyncError { code } => HostEvent::SyncError {
            code,
            doc_id: Some(notice.doc_id),
            scope: None,
            fatal: false,
            recovered_id,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::KeptCopy;

    #[test]
    fn conflict_notices_become_conflict_events_with_their_kept_copy() {
        let doc_id = Uuid::from_u128(1);
        let kept = |reason: &str| {
            Some(KeptCopy {
                recovered_id: 7,
                reason: reason.into(),
            })
        };
        assert_eq!(
            noticed(DocNotice {
                doc_id,
                event: DocEvent::ConflictDetected,
                kept: kept("delete_wins")
            }),
            HostEvent::Conflict {
                doc_id,
                reason: "delete_wins".into(),
                recovered_id: Some(7),
                paths: vec![]
            }
        );
        assert_eq!(
            noticed(DocNotice {
                doc_id,
                event: DocEvent::FieldConflict {
                    paths: vec!["/s".into()]
                },
                kept: kept("field_conflict")
            }),
            HostEvent::Conflict {
                doc_id,
                reason: "field_conflict".into(),
                recovered_id: Some(7),
                paths: vec!["/s".into()]
            }
        );
        assert_eq!(
            noticed(DocNotice {
                doc_id,
                event: DocEvent::SyncError {
                    code: "validation".into()
                },
                kept: None
            }),
            HostEvent::SyncError {
                code: "validation".into(),
                doc_id: Some(doc_id),
                scope: None,
                fatal: false,
                recovered_id: None
            }
        );
        assert_eq!(
            noticed(DocNotice {
                doc_id,
                event: DocEvent::SyncError {
                    code: "forbidden".into()
                },
                kept: kept("delete_refused")
            }),
            HostEvent::SyncError {
                code: "forbidden".into(),
                doc_id: Some(doc_id),
                scope: None,
                fatal: false,
                recovered_id: Some(7)
            }
        );
        for (kept_copy, recovered_id) in [(kept("delete_superseded"), Some(7)), (None, None)] {
            assert_eq!(
                noticed(DocNotice {
                    doc_id,
                    event: DocEvent::DeleteSuperseded,
                    kept: kept_copy
                }),
                HostEvent::Conflict {
                    doc_id,
                    reason: "delete_superseded".into(),
                    recovered_id,
                    paths: vec![]
                }
            );
        }
    }

    #[test]
    fn a_full_queue_collapses_document_events_into_one_database_changed() {
        let queue = EventQueue::default();
        let deleted = |n: u128| HostEvent::DocumentDeleted {
            doc_id: Uuid::from_u128(n),
            origin: Origin::Server,
        };
        queue.push(HostEvent::SyncStarted);
        for n in 1..QUEUE_CAP as u128 {
            queue.push(deleted(n));
        }
        queue.push(deleted(0));
        queue.push(HostEvent::SyncCompleted);
        queue.push(deleted(1));
        assert_eq!(
            queue.take(),
            vec![
                HostEvent::SyncStarted,
                HostEvent::DatabaseChanged,
                HostEvent::SyncCompleted
            ],
            "document events collapse; lifecycle events stay in order"
        );
        queue.push(deleted(2));
        assert_eq!(
            queue.take(),
            vec![deleted(2)],
            "after a take, events flow again"
        );
    }

    #[test]
    fn dial_attempts_never_evict_a_kept_copy_notice() {
        let queue = EventQueue::default();
        let conflict = HostEvent::Conflict {
            doc_id: Uuid::from_u128(1),
            reason: "field_conflict".into(),
            recovered_id: Some(7),
            paths: vec![],
        };
        queue.push(conflict.clone());
        for attempt in 0..=QUEUE_CAP as u32 {
            queue.push(HostEvent::ConnectionAttempted { attempt });
        }
        assert_eq!(
            queue.take(),
            vec![
                conflict,
                HostEvent::ConnectionAttempted {
                    attempt: QUEUE_CAP as u32
                }
            ]
        );
    }
}
