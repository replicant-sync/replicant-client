//! Driver contract: every effect that touches the DB answers with the input named in its
//! doc comment. Timers are fire-and-forget; the core ignores timers it no longer expects.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use uuid::Uuid;

#[allow(unused_imports)]
use super::backoff::{catch_up_retry_delay, connect_delay, doc_retry_delay, Jitter};
use super::doc::InFlight;
#[allow(unused_imports)]
use super::types::{Change, DocEnvelope, Scope, Seq, ServerError, Upload};

pub const MAX_IN_FLIGHT: usize = 8;
pub const PAGE_LIMIT: u32 = 500;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
const STABLE_AFTER: Duration = Duration::from_secs(60);
const QUIET: Duration = Duration::from_millis(200);
const QUIET_CAP: Duration = Duration::from_secs(1);
const HALT_RETRY: Duration = Duration::from_secs(300);
const MAX_CATCH_UP_FAILURES: u32 = 3;

/// Why the connection is halted and will not retry on its own schedule.
#[derive(Debug, Clone, PartialEq)]
pub enum HaltReason {
    NotEnrolled,
    AuthInvalid,
    UpdateRequired,
    AccountDisabled,
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionView {
    Idle,
    Disconnected,
    Connecting,
    Connected,
    Halted(HaltReason),
    Stopped,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SyncView {
    Idle,
    CatchingUp,
    Live,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EngineState {
    pub connection: ConnectionView,
    pub sync: SyncView,
}

/// Identifies a scheduled timer so it can be cancelled or matched on fire.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TimerId {
    ConnectTimeout,
    Reconnect,
    StableReset,
    Heartbeat,
    HeartbeatTimeout(u64),
    Request(u64),
    Pump,
    PumpCap,
    DocRetry(Uuid),
    CatchUpRetry(Scope),
    HaltRetry,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Join,
    GetChangesSince {
        scope: Scope,
        cursor: Seq,
        limit: u32,
    },
    GetSnapshot {
        scope: Scope,
        page_token: Option<String>,
    },
    Upload(Upload),
    GetDocument {
        doc_id: Uuid,
    },
    Heartbeat,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    Joined,
    Changes {
        changes: Vec<Change>,
        next_cursor: Seq,
        has_more: bool,
    },
    SnapshotPage {
        docs: Vec<DocEnvelope>,
        snapshot_seq: Seq,
        next_page_token: Option<String>,
    },
    Uploaded(DocEnvelope),
    Document(DocEnvelope),
    HeartbeatOk,
}

/// Tags an in-flight changes/snapshot apply so the driver's `Applied` reply can be matched back.
#[derive(Debug, Clone, PartialEq)]
pub enum ApplyTag {
    Page(u64),
    Push,
    SnapshotPage(u64),
    SnapshotFinish,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BuildOutcome {
    Send { upload: Upload, inflight: InFlight },
    NeedsServerCopy,
    Nothing,
    SettledLocally { rows_remain: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SettleOutcome {
    Done {
        rows_remain: bool,
    },
    FetchServerCopy,
    Retry {
        after_ms: Option<u64>,
        mismatch: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    Start {
        has_credentials: bool,
    },
    Reconnect,
    CredentialsChanged {
        has_credentials: bool,
    },
    OutboxChanged,
    Shutdown,
    SocketOpened,
    SocketClosed,
    Reply {
        req: u64,
        result: Result<Response, ServerError>,
    },
    Push(Change),
    Timer(TimerId),
    Cursors(Vec<(Scope, Seq)>),
    Applied {
        scope: Scope,
        tag: ApplyTag,
    },
    PendingDocs(Vec<Uuid>),
    UploadBuilt {
        doc_id: Uuid,
        outcome: BuildOutcome,
    },
    Settled {
        doc_id: Uuid,
        outcome: SettleOutcome,
    },
    ServerCopyApplied {
        doc_id: Uuid,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Lifecycle {
    ConnectionAttempted,
    ConnectionSucceeded,
    ConnectionLost,
    SyncStarted,
    SyncCompleted,
    SyncError {
        code: String,
        scope: Option<Scope>,
        doc_id: Option<Uuid>,
        fatal: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    OpenSocket,
    CloseSocket,
    /// Answered with `Input::CredentialsChanged`.
    CheckCredentials,
    Send {
        req: u64,
        request: Request,
    },
    Schedule {
        timer: TimerId,
        after: Duration,
    },
    Cancel(TimerId),
    /// Answered with `Input::Cursors`.
    LoadCursors,
    /// Answered with `Input::Applied` echoing `tag`.
    ApplyChanges {
        scope: Scope,
        changes: Vec<Change>,
        new_cursor: Seq,
        tag: ApplyTag,
    },
    /// Answered with `Input::Applied` echoing `tag`.
    ApplySnapshotPage {
        scope: Scope,
        docs: Vec<DocEnvelope>,
        tag: ApplyTag,
    },
    /// Answered with `Input::Applied { tag: ApplyTag::SnapshotFinish, .. }`.
    FinishSnapshot {
        scope: Scope,
        seen: Vec<Uuid>,
        snapshot_seq: Seq,
    },
    /// Answered with `Input::PendingDocs`.
    LoadPending,
    /// Answered with `Input::UploadBuilt`.
    BuildUpload {
        doc_id: Uuid,
    },
    /// Answered with `Input::Settled`.
    SettleUpload {
        doc_id: Uuid,
        inflight: InFlight,
        reply: Result<DocEnvelope, ServerError>,
        mismatch_attempts: u32,
    },
    /// Answered with `Input::ServerCopyApplied`.
    ApplyServerCopy {
        doc_id: Uuid,
        doc: Option<DocEnvelope>,
    },
    /// The server reported the document as deleted; answered with `ServerCopyApplied`.
    ApplyServerDeleted {
        doc_id: Uuid,
        seq: Seq,
    },
    Emit(Lifecycle),
    SetState(EngineState),
}

#[derive(Debug, Clone, PartialEq)]
enum ScopeSync {
    /// Waiting for a changes page; pushes are buffered.
    Requesting {
        req: u64,
    },
    /// Page handed to the driver; `has_more` decides the next step.
    Applying {
        req: u64,
        has_more: bool,
    },
    SnapshotRequesting {
        req: u64,
    },
    SnapshotApplying {
        req: u64,
        next_page: Option<String>,
    },
    SnapshotFinishing {
        snapshot_seq: Seq,
    },
    RetryWait,
    Live,
}

#[derive(Debug, Clone, PartialEq)]
enum Pending {
    Changes { scope: Scope },
    Snapshot { scope: Scope },
    Upload { doc_id: Uuid },
    Document { doc_id: Uuid },
    Heartbeat,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    LoadingCursors,
    CatchingUp,
    Live,
}

#[derive(Debug, Default)]
struct Session {
    phase: Option<Phase>,
    synced_once: bool,
    cursors: HashMap<Scope, Seq>,
    scopes: HashMap<Scope, ScopeSync>,
    buffered: HashMap<Scope, Vec<Change>>,
    snapshot_seen: HashMap<Scope, (Vec<Uuid>, Seq)>,
    requests: HashMap<u64, Pending>,
    in_flight: HashMap<Uuid, InFlight>,
    building: HashSet<Uuid>,
    backing_off: HashSet<Uuid>,
    doc_failures: HashMap<Uuid, u32>,
    mismatch_attempts: HashMap<Uuid, u32>,
    catch_up_failures: u32,
    pump_scheduled: bool,
}

#[derive(Debug)]
enum Conn {
    Idle,
    Disconnected,
    Connecting { join_req: Option<u64> },
    Connected(Session),
    Halted(HaltReason),
    Stopped,
}

pub struct Core {
    me: Uuid,
    scope_names: Vec<Scope>,
    conn: Conn,
    attempt: u32,
    jitter: Jitter,
    next_req: u64,
}

impl Core {
    pub fn new(me: Uuid, scopes: Vec<Scope>, seed: u64) -> Self {
        Core {
            me,
            scope_names: scopes,
            conn: Conn::Idle,
            attempt: 0,
            jitter: Jitter::new(seed),
            next_req: 1,
        }
    }

    pub fn state(&self) -> EngineState {
        let (connection, sync) = match &self.conn {
            Conn::Idle => (ConnectionView::Idle, SyncView::Idle),
            Conn::Disconnected => (ConnectionView::Disconnected, SyncView::Idle),
            Conn::Connecting { .. } => (ConnectionView::Connecting, SyncView::Idle),
            Conn::Connected(s) => (
                ConnectionView::Connected,
                match s.phase {
                    Some(Phase::Live) => SyncView::Live,
                    Some(Phase::CatchingUp) => SyncView::CatchingUp,
                    _ => SyncView::Idle,
                },
            ),
            Conn::Halted(r) => (ConnectionView::Halted(r.clone()), SyncView::Idle),
            Conn::Stopped => (ConnectionView::Stopped, SyncView::Idle),
        };
        EngineState { connection, sync }
    }

    fn alloc_req(&mut self) -> u64 {
        let r = self.next_req;
        self.next_req += 1;
        r
    }

    pub fn step(&mut self, input: Input) -> Vec<Effect> {
        let mut fx = Vec::new();
        let before = self.state();
        if !matches!(self.conn, Conn::Idle)
            || matches!(input, Input::Start { .. } | Input::Shutdown)
        {
            self.handle(input, &mut fx);
        }
        let after = self.state();
        if after != before {
            fx.push(Effect::SetState(after));
        }
        fx
    }

    fn handle(&mut self, _input: Input, _fx: &mut Vec<Effect>) {}
}

#[cfg(test)]
pub(crate) mod harness {
    use super::*;

    pub const ME: Uuid = Uuid::from_u128(0xA);

    pub fn core() -> Core {
        Core::new(ME, vec!["own".into(), "collection:curated".into()], 7)
    }

    pub fn sends(fx: &[Effect]) -> Vec<(u64, Request)> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Send { req, request } => Some((*req, request.clone())),
                _ => None,
            })
            .collect()
    }

    pub fn emitted(fx: &[Effect]) -> Vec<Lifecycle> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Emit(l) => Some(l.clone()),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod skeleton_tests {
    use super::harness::*;
    use super::*;

    #[test]
    fn new_core_is_idle() {
        let c = core();
        assert_eq!(
            c.state(),
            EngineState {
                connection: ConnectionView::Idle,
                sync: SyncView::Idle
            }
        );
    }

    #[test]
    fn inputs_before_start_are_ignored() {
        let mut c = core();
        assert!(c.step(Input::OutboxChanged).is_empty());
        assert!(c.step(Input::SocketClosed).is_empty());
    }
}
