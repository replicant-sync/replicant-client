//! Driver contract: every effect that touches the DB answers with the input named in its
//! doc comment. Timers are fire-and-forget; the core ignores timers it no longer expects.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use uuid::Uuid;

use super::backoff::{
    catch_up_retry_delay, connect_delay, doc_retry_delay, Jitter, MIN_CONNECT_DELAY,
};
use super::doc::InFlight;
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
const UNREADABLE_PUSHES_BEFORE_ERROR: u32 = 3;

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
    /// Carries the req of the scope's last snapshot page.
    SnapshotFinish(u64),
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
    /// `gen` echoes the `OpenSocket` it answers; events from older sockets are ignored.
    SocketOpened {
        gen: u64,
    },
    SocketClosed {
        gen: u64,
    },
    /// The server refused the websocket upgrade (HTTP 426 → `update_required`).
    ConnectRefused {
        gen: u64,
        error: ServerError,
    },
    Reply {
        req: u64,
        result: Result<Response, ServerError>,
    },
    Push(Change),
    /// A push that did not decode; it may have been a change, so its scope catches up.
    UnreadablePush {
        scope: Option<Scope>,
    },
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
    OpenSocket {
        gen: u64,
    },
    CloseSocket {
        gen: u64,
    },
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
    /// Answered with `Input::Applied` echoing `tag` (an `ApplyTag::SnapshotFinish`).
    FinishSnapshot {
        scope: Scope,
        seen: Vec<Uuid>,
        snapshot_seq: Seq,
        tag: ApplyTag,
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
    /// Driver deletes the subscription row; no reply.
    DropSubscription {
        scope: Scope,
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
        req: u64,
        snapshot_seq: Seq,
    },
    RetryWait,
    Live,
    /// Server rejected the subscription; treated as done, pushes ignored.
    Dropped,
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
    fetching: HashSet<Uuid>,
    settling: HashSet<Uuid>,
    doc_failures: HashMap<Uuid, u32>,
    mismatch_attempts: HashMap<Uuid, u32>,
    catch_up_failures: HashMap<Scope, u32>,
    unreadable_pushes: HashMap<Option<Scope>, u32>,
    /// Scopes that took an unreadable push while not `Live`; the push may be the one change
    /// `finish_scope` would otherwise seal past, so it rechecks once more before going `Live`.
    recheck_before_live: HashSet<Scope>,
    pump_scheduled: bool,
    pump_deferred: bool,
}

impl Session {
    fn slots_used(&self) -> usize {
        self.in_flight.len() + self.building.len() + self.fetching.len() + self.settling.len()
    }
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
    scope_names: Vec<Scope>,
    conn: Conn,
    attempt: u32,
    jitter: Jitter,
    next_req: u64,
    socket_gen: u64,
}

impl Core {
    pub fn new(scopes: Vec<Scope>, seed: u64) -> Self {
        Core {
            scope_names: scopes,
            conn: Conn::Idle,
            attempt: 0,
            jitter: Jitter::new(seed),
            next_req: 1,
            socket_gen: 0,
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

    fn handle(&mut self, input: Input, fx: &mut Vec<Effect>) {
        if matches!(self.conn, Conn::Stopped) {
            return;
        }
        match input {
            Input::Shutdown => {
                self.close_socket(fx);
                self.conn = Conn::Stopped;
            }
            Input::Start { has_credentials } => {
                if matches!(self.conn, Conn::Idle) {
                    if has_credentials {
                        self.connect_now(fx);
                    } else {
                        self.halt(HaltReason::NotEnrolled, fx);
                    }
                }
            }
            Input::Reconnect => match self.conn {
                Conn::Disconnected => {
                    fx.push(Effect::Cancel(TimerId::Reconnect));
                    self.attempt = 0;
                    self.connect_now(fx);
                }
                Conn::Halted(_) => {
                    fx.push(Effect::Cancel(TimerId::HaltRetry));
                    self.attempt = 0;
                    self.connect_now(fx);
                }
                _ => {}
            },
            Input::CredentialsChanged { has_credentials } => {
                if let Conn::Halted(reason) = &self.conn {
                    if has_credentials {
                        self.connect_now(fx);
                    } else if matches!(reason, HaltReason::NotEnrolled | HaltReason::AuthInvalid) {
                        fx.push(Effect::Schedule {
                            timer: TimerId::HaltRetry,
                            after: HALT_RETRY,
                        });
                    }
                }
            }
            Input::SocketOpened { gen } | Input::SocketClosed { gen } if gen != self.socket_gen => {
            }
            Input::SocketOpened { .. } => {
                if let Conn::Connecting { join_req: None } = self.conn {
                    let req = self.alloc_req();
                    self.conn = Conn::Connecting {
                        join_req: Some(req),
                    };
                    fx.push(Effect::Send {
                        req,
                        request: Request::Join,
                    });
                }
            }
            Input::SocketClosed { .. } => match self.conn {
                Conn::Connecting { .. } => self.fail_connect(None, fx),
                Conn::Connected(_) => self.lose_connection(fx),
                _ => {}
            },
            Input::ConnectRefused { gen, .. } if gen != self.socket_gen => {}
            Input::ConnectRefused { error, .. } => {
                if let Conn::Connecting { .. } = self.conn {
                    match Self::halt_reason_for(&error) {
                        Some(reason) => {
                            fx.push(Effect::Cancel(TimerId::ConnectTimeout));
                            self.halt(reason, fx);
                        }
                        None => self.fail_connect(error.retry_after_ms, fx),
                    }
                }
            }
            Input::Reply { req, result } => self.on_reply(req, result, fx),
            Input::Timer(timer) => self.on_timer(timer, fx),
            Input::Cursors(list) => self.on_cursors(list, fx),
            other => self.on_session_input(other, fx),
        }
    }

    fn connect_now(&mut self, fx: &mut Vec<Effect>) {
        self.conn = Conn::Connecting { join_req: None };
        self.socket_gen += 1;
        fx.push(Effect::Emit(Lifecycle::ConnectionAttempted));
        fx.push(Effect::OpenSocket {
            gen: self.socket_gen,
        });
        fx.push(Effect::Schedule {
            timer: TimerId::ConnectTimeout,
            after: CONNECT_TIMEOUT,
        });
    }

    fn close_socket(&self, fx: &mut Vec<Effect>) {
        fx.push(Effect::CloseSocket {
            gen: self.socket_gen,
        });
    }

    fn fail_connect(&mut self, retry_after_ms: Option<u64>, fx: &mut Vec<Effect>) {
        fx.push(Effect::Cancel(TimerId::ConnectTimeout));
        let delay = match retry_after_ms {
            Some(ms) => Duration::from_millis(ms).max(MIN_CONNECT_DELAY),
            None => connect_delay(self.attempt, self.jitter.next_unit()),
        };
        self.attempt = self.attempt.saturating_add(1);
        self.conn = Conn::Disconnected;
        fx.push(Effect::Schedule {
            timer: TimerId::Reconnect,
            after: delay,
        });
    }

    fn lose_connection(&mut self, fx: &mut Vec<Effect>) {
        fx.push(Effect::Emit(Lifecycle::ConnectionLost));
        self.close_socket(fx);
        fx.push(Effect::Cancel(TimerId::Heartbeat));
        fx.push(Effect::Cancel(TimerId::StableReset));
        let delay = connect_delay(self.attempt, self.jitter.next_unit());
        self.attempt = self.attempt.saturating_add(1);
        self.conn = Conn::Disconnected;
        fx.push(Effect::Schedule {
            timer: TimerId::Reconnect,
            after: delay,
        });
    }

    fn halt(&mut self, reason: HaltReason, fx: &mut Vec<Effect>) {
        let code = match &reason {
            HaltReason::NotEnrolled => "not_enrolled".to_string(),
            HaltReason::AuthInvalid => "auth_invalid".to_string(),
            HaltReason::UpdateRequired => "update_required".to_string(),
            HaltReason::AccountDisabled => "account_disabled".to_string(),
            HaltReason::Other(c) => c.clone(),
        };
        fx.push(Effect::Emit(Lifecycle::SyncError {
            code,
            scope: None,
            doc_id: None,
            fatal: true,
        }));
        if matches!(reason, HaltReason::NotEnrolled | HaltReason::AuthInvalid) {
            fx.push(Effect::Schedule {
                timer: TimerId::HaltRetry,
                after: HALT_RETRY,
            });
        }
        self.conn = Conn::Halted(reason);
    }

    fn halt_reason_for(e: &ServerError) -> Option<HaltReason> {
        match e.code.as_str() {
            "update_required" => Some(HaltReason::UpdateRequired),
            "auth_invalid" => Some(HaltReason::AuthInvalid),
            "account_disabled" => Some(HaltReason::AccountDisabled),
            code if e.is_fatal => Some(HaltReason::Other(code.to_string())),
            _ => None,
        }
    }

    fn on_joined(&mut self, fx: &mut Vec<Effect>) {
        fx.push(Effect::Cancel(TimerId::ConnectTimeout));
        self.conn = Conn::Connected(Session {
            phase: Some(Phase::LoadingCursors),
            ..Default::default()
        });
        fx.push(Effect::Emit(Lifecycle::ConnectionSucceeded));
        fx.push(Effect::Schedule {
            timer: TimerId::Heartbeat,
            after: HEARTBEAT_EVERY,
        });
        fx.push(Effect::Schedule {
            timer: TimerId::StableReset,
            after: STABLE_AFTER,
        });
        fx.push(Effect::LoadCursors);
    }

    fn on_reply(&mut self, req: u64, result: Result<Response, ServerError>, fx: &mut Vec<Effect>) {
        if let Conn::Connecting { join_req: Some(j) } = self.conn {
            if j == req {
                match result {
                    Ok(_) => self.on_joined(fx),
                    Err(e) => match Self::halt_reason_for(&e) {
                        Some(reason) => {
                            fx.push(Effect::Cancel(TimerId::ConnectTimeout));
                            self.close_socket(fx);
                            self.halt(reason, fx);
                        }
                        None => {
                            // The host can tell the user to fix the device clock.
                            if e.code == "clock_skew" {
                                fx.push(Effect::Emit(Lifecycle::SyncError {
                                    code: e.code.clone(),
                                    scope: None,
                                    doc_id: None,
                                    fatal: false,
                                }));
                            }
                            self.close_socket(fx);
                            self.fail_connect(e.retry_after_ms, fx);
                        }
                    },
                }
            }
            return;
        }
        let pending = match &mut self.conn {
            Conn::Connected(s) => s.requests.remove(&req),
            _ => None,
        };
        let Some(pending) = pending else { return };
        fx.push(Effect::Cancel(TimerId::Request(req)));
        match pending {
            Pending::Heartbeat => fx.push(Effect::Cancel(TimerId::HeartbeatTimeout(req))),
            other => self.on_session_reply(req, other, result, fx),
        }
    }

    fn on_timer(&mut self, timer: TimerId, fx: &mut Vec<Effect>) {
        match (&mut self.conn, timer) {
            (Conn::Disconnected, TimerId::Reconnect) => self.connect_now(fx),
            (Conn::Connecting { .. }, TimerId::ConnectTimeout) => {
                self.close_socket(fx);
                self.fail_connect(None, fx);
            }
            (Conn::Halted(_), TimerId::HaltRetry) => fx.push(Effect::CheckCredentials),
            (Conn::Connected(_), TimerId::StableReset) => self.attempt = 0,
            (Conn::Connected(s), TimerId::Heartbeat) => {
                let req = self.next_req;
                self.next_req += 1;
                s.requests.insert(req, Pending::Heartbeat);
                fx.push(Effect::Send {
                    req,
                    request: Request::Heartbeat,
                });
                fx.push(Effect::Schedule {
                    timer: TimerId::HeartbeatTimeout(req),
                    after: HEARTBEAT_TIMEOUT,
                });
                fx.push(Effect::Schedule {
                    timer: TimerId::Heartbeat,
                    after: HEARTBEAT_EVERY,
                });
            }
            (Conn::Connected(s), TimerId::HeartbeatTimeout(req)) => {
                if s.requests.remove(&req).is_some() {
                    self.lose_connection(fx);
                }
            }
            (Conn::Connected(_), other) => self.on_session_timer(other, fx),
            _ => {}
        }
    }

    fn on_cursors(&mut self, list: Vec<(Scope, Seq)>, fx: &mut Vec<Effect>) {
        let Conn::Connected(s) = &mut self.conn else {
            return;
        };
        if s.phase != Some(Phase::LoadingCursors) {
            return;
        }
        for name in &self.scope_names {
            let cursor = list.iter().find(|(n, _)| n == name).map_or(0, |(_, c)| *c);
            s.cursors.insert(name.clone(), cursor);
        }
        s.phase = Some(Phase::CatchingUp);
        fx.push(Effect::Emit(Lifecycle::SyncStarted));
        for name in self.scope_names.clone() {
            self.start_catch_up(&name, fx);
        }
        self.request_pump(fx);
    }

    fn session(&mut self) -> Option<&mut Session> {
        match &mut self.conn {
            Conn::Connected(s) => Some(s),
            _ => None,
        }
    }

    fn start_catch_up(&mut self, scope: &str, fx: &mut Vec<Effect>) {
        let cursor = match self.session() {
            Some(s) => *s.cursors.get(scope).unwrap_or(&0),
            None => return,
        };
        if cursor == 0 {
            let req = self.alloc_req();
            let Some(s) = self.session() else { return };
            s.scopes
                .insert(scope.to_string(), ScopeSync::SnapshotRequesting { req });
            s.snapshot_seen.insert(scope.to_string(), (Vec::new(), 0));
            s.requests.insert(
                req,
                Pending::Snapshot {
                    scope: scope.to_string(),
                },
            );
            fx.push(Effect::Send {
                req,
                request: Request::GetSnapshot {
                    scope: scope.to_string(),
                    page_token: None,
                },
            });
            fx.push(Effect::Schedule {
                timer: TimerId::Request(req),
                after: REQUEST_TIMEOUT,
            });
        } else {
            self.request_changes(scope, cursor, fx);
        }
    }

    fn request_changes(&mut self, scope: &str, cursor: Seq, fx: &mut Vec<Effect>) {
        let req = self.alloc_req();
        let Some(s) = self.session() else { return };
        s.scopes
            .insert(scope.to_string(), ScopeSync::Requesting { req });
        s.requests.insert(
            req,
            Pending::Changes {
                scope: scope.to_string(),
            },
        );
        fx.push(Effect::Send {
            req,
            request: Request::GetChangesSince {
                scope: scope.to_string(),
                cursor,
                limit: PAGE_LIMIT,
            },
        });
        fx.push(Effect::Schedule {
            timer: TimerId::Request(req),
            after: REQUEST_TIMEOUT,
        });
    }

    fn request_snapshot_page(
        &mut self,
        scope: &str,
        page_token: Option<String>,
        fx: &mut Vec<Effect>,
    ) {
        let req = self.alloc_req();
        let Some(s) = self.session() else { return };
        s.scopes
            .insert(scope.to_string(), ScopeSync::SnapshotRequesting { req });
        s.requests.insert(
            req,
            Pending::Snapshot {
                scope: scope.to_string(),
            },
        );
        fx.push(Effect::Send {
            req,
            request: Request::GetSnapshot {
                scope: scope.to_string(),
                page_token,
            },
        });
        fx.push(Effect::Schedule {
            timer: TimerId::Request(req),
            after: REQUEST_TIMEOUT,
        });
    }

    fn fail_catch_up(&mut self, scope: &str, err: Option<&ServerError>, fx: &mut Vec<Effect>) {
        let count = {
            let Some(s) = self.session() else { return };
            let count = s.catch_up_failures.entry(scope.to_string()).or_insert(0);
            *count += 1;
            *count
        };
        if count >= MAX_CATCH_UP_FAILURES {
            self.lose_connection(fx);
            return;
        }
        let delay = match err.and_then(|e| e.retry_after_ms) {
            Some(ms) => Duration::from_millis(ms),
            None => catch_up_retry_delay(count),
        };
        let Some(s) = self.session() else { return };
        s.scopes.insert(scope.to_string(), ScopeSync::RetryWait);
        fx.push(Effect::Schedule {
            timer: TimerId::CatchUpRetry(scope.to_string()),
            after: delay,
        });
    }

    /// A fatal server error ends the Connected period like `lose_connection`, then halts
    /// instead of scheduling a reconnect.
    fn catch_up_fatal(&mut self, reason: HaltReason, fx: &mut Vec<Effect>) {
        fx.push(Effect::Emit(Lifecycle::ConnectionLost));
        self.close_socket(fx);
        fx.push(Effect::Cancel(TimerId::Heartbeat));
        fx.push(Effect::Cancel(TimerId::StableReset));
        self.halt(reason, fx);
    }

    fn drop_subscription(&mut self, scope: &str, code: String, fx: &mut Vec<Effect>) {
        fx.push(Effect::Emit(Lifecycle::SyncError {
            code,
            scope: Some(scope.to_string()),
            doc_id: None,
            fatal: false,
        }));
        fx.push(Effect::DropSubscription {
            scope: scope.to_string(),
        });
        self.scope_names.retain(|name| name != scope);
        let Some(s) = self.session() else { return };
        s.scopes.insert(scope.to_string(), ScopeSync::Dropped);
        self.check_all_live(fx);
    }

    fn on_session_reply(
        &mut self,
        req: u64,
        pending: Pending,
        result: Result<Response, ServerError>,
        fx: &mut Vec<Effect>,
    ) {
        match pending {
            Pending::Changes { scope } => match result {
                Ok(Response::Changes {
                    changes,
                    next_cursor,
                    has_more,
                }) => {
                    let Some(s) = self.session() else { return };
                    s.cursors.insert(scope.clone(), next_cursor);
                    s.scopes
                        .insert(scope.clone(), ScopeSync::Applying { req, has_more });
                    fx.push(Effect::ApplyChanges {
                        scope,
                        changes,
                        new_cursor: next_cursor,
                        tag: ApplyTag::Page(req),
                    });
                }
                Err(e) if e.code == "cursor_too_old" => {
                    if let Some(s) = self.session() {
                        s.snapshot_seen.insert(scope.clone(), (Vec::new(), 0));
                    }
                    self.request_snapshot_page(&scope, None, fx);
                }
                Err(e) if Self::halt_reason_for(&e).is_some() => {
                    let reason = Self::halt_reason_for(&e).unwrap();
                    self.catch_up_fatal(reason, fx);
                }
                Err(e) if e.code == "subscription_forbidden" => {
                    self.drop_subscription(&scope, e.code, fx);
                }
                Err(e) => self.fail_catch_up(&scope, Some(&e), fx),
                Ok(_) => self.fail_catch_up(&scope, None, fx),
            },
            Pending::Snapshot { scope } => match result {
                Ok(Response::SnapshotPage {
                    docs,
                    snapshot_seq,
                    next_page_token,
                }) => {
                    let Some(s) = self.session() else { return };
                    let entry = s
                        .snapshot_seen
                        .entry(scope.clone())
                        .or_insert((Vec::new(), 0));
                    entry.0.extend(docs.iter().map(|d| d.doc_id));
                    if entry.1 == 0 {
                        entry.1 = snapshot_seq; // fixed by the server before page 1
                    }
                    s.scopes.insert(
                        scope.clone(),
                        ScopeSync::SnapshotApplying {
                            req,
                            next_page: next_page_token,
                        },
                    );
                    fx.push(Effect::ApplySnapshotPage {
                        scope,
                        docs,
                        tag: ApplyTag::SnapshotPage(req),
                    });
                }
                Err(e) if Self::halt_reason_for(&e).is_some() => {
                    let reason = Self::halt_reason_for(&e).unwrap();
                    self.catch_up_fatal(reason, fx);
                }
                Err(e) if e.code == "subscription_forbidden" => {
                    self.drop_subscription(&scope, e.code, fx);
                }
                Err(e) => self.fail_catch_up(&scope, Some(&e), fx),
                Ok(_) => self.fail_catch_up(&scope, None, fx),
            },
            Pending::Upload { doc_id } => {
                if let Some(reason) = result.as_ref().err().and_then(Self::halt_reason_for) {
                    self.catch_up_fatal(reason, fx);
                    return;
                }
                let Some(s) = self.session() else { return };
                let Some(inflight) = s.in_flight.remove(&doc_id) else {
                    return;
                };
                s.settling.insert(doc_id);
                let mismatch_attempts = *s.mismatch_attempts.get(&doc_id).unwrap_or(&0);
                let reply = match result {
                    Ok(Response::Uploaded(doc)) => Ok(doc),
                    Ok(other) => Err(ServerError::new(&format!("unexpected_response:{other:?}"))),
                    Err(e) => Err(e),
                };
                fx.push(Effect::SettleUpload {
                    doc_id,
                    inflight,
                    reply,
                    mismatch_attempts,
                });
                self.refill(fx);
            }
            Pending::Document { doc_id } => {
                if let Some(reason) = result.as_ref().err().and_then(Self::halt_reason_for) {
                    self.catch_up_fatal(reason, fx);
                    return;
                }
                // The doc stays in `fetching` until `ServerCopyApplied` unless we back off.
                match result {
                    Ok(Response::Document(doc)) => fx.push(Effect::ApplyServerCopy {
                        doc_id,
                        doc: Some(doc),
                    }),
                    Err(e) if e.code == "deleted" => fx.push(Effect::ApplyServerDeleted {
                        doc_id,
                        seq: e.current_seq.unwrap_or(0),
                    }),
                    Err(e) if e.code == "not_found" => {
                        fx.push(Effect::ApplyServerCopy { doc_id, doc: None })
                    }
                    other => {
                        if let Some(s) = self.session() {
                            s.fetching.remove(&doc_id);
                        }
                        let after_ms = other.err().and_then(|e| e.retry_after_ms);
                        self.back_off_doc(doc_id, after_ms, fx);
                    }
                }
                self.refill(fx);
            }
            Pending::Heartbeat => {}
        }
    }

    fn on_session_timer(&mut self, timer: TimerId, fx: &mut Vec<Effect>) {
        match timer {
            TimerId::CatchUpRetry(scope) => {
                let waiting = self
                    .session()
                    .is_some_and(|s| s.scopes.get(&scope) == Some(&ScopeSync::RetryWait));
                if waiting {
                    self.start_catch_up(&scope, fx);
                }
            }
            TimerId::Request(req) => {
                let pending = self.session().and_then(|s| s.requests.remove(&req));
                match pending {
                    Some(Pending::Changes { scope }) | Some(Pending::Snapshot { scope }) => {
                        self.fail_catch_up(&scope, None, fx)
                    }
                    Some(other) => self.on_request_timeout(other, fx),
                    None => {}
                }
            }
            other => self.on_upload_timer(other, fx),
        }
    }

    fn on_session_input(&mut self, input: Input, fx: &mut Vec<Effect>) {
        if !matches!(self.conn, Conn::Connected(_)) {
            return;
        }
        match input {
            Input::Push(change) => self.on_push(change, fx),
            Input::UnreadablePush { scope } => self.on_unreadable_push(scope, fx),
            Input::Applied { scope, tag } => self.on_applied(scope, tag, fx),
            other => self.on_upload_input(other, fx),
        }
    }

    fn on_push(&mut self, change: Change, fx: &mut Vec<Effect>) {
        let Some(s) = self.session() else { return };
        let scope = change.scope.clone();
        match s.scopes.get(&scope) {
            Some(ScopeSync::Live) => {}
            Some(ScopeSync::Dropped) => return,
            Some(_) => {
                s.buffered.entry(scope).or_default().push(change);
                return;
            }
            None => return,
        }
        let cursor = *s.cursors.get(&scope).unwrap_or(&0);
        if change.seq <= cursor {
            return;
        }
        if change.prev_seq <= cursor {
            s.cursors.insert(scope.clone(), change.seq);
            let new_cursor = change.seq;
            fx.push(Effect::ApplyChanges {
                scope,
                changes: vec![change],
                new_cursor,
                tag: ApplyTag::Push,
            });
        } else {
            self.start_catch_up(&scope, fx);
        }
    }

    fn on_unreadable_push(&mut self, scope: Option<Scope>, fx: &mut Vec<Effect>) {
        let names = self.scope_names.clone();
        let Some(s) = self.session() else { return };
        let count = s.unreadable_pushes.entry(scope.clone()).or_insert(0);
        *count += 1;
        if *count == UNREADABLE_PUSHES_BEFORE_ERROR {
            fx.push(Effect::Emit(Lifecycle::SyncError {
                code: "protocol_error".into(),
                scope: scope.clone(),
                doc_id: None,
                fatal: false,
            }));
        }
        let mut live = Vec::new();
        for name in names {
            if scope.as_ref().is_some_and(|wanted| wanted != &name) {
                continue;
            }
            match s.scopes.get(&name) {
                Some(ScopeSync::Live) => live.push(name),
                Some(ScopeSync::Dropped) | None => {}
                Some(_) => {
                    s.recheck_before_live.insert(name);
                }
            }
        }
        for name in live {
            self.start_catch_up(&name, fx);
        }
    }

    fn on_applied(&mut self, scope: Scope, tag: ApplyTag, fx: &mut Vec<Effect>) {
        let Some(s) = self.session() else { return };
        let state = s.scopes.get(&scope).cloned();
        match (state, tag) {
            (Some(ScopeSync::Applying { req, has_more }), ApplyTag::Page(t)) if req == t => {
                if has_more {
                    self.start_catch_up(&scope, fx);
                } else {
                    self.finish_scope(&scope, fx);
                }
            }
            (Some(ScopeSync::SnapshotApplying { req, next_page }), ApplyTag::SnapshotPage(t))
                if req == t =>
            {
                match next_page {
                    Some(token) => self.request_snapshot_page(&scope, Some(token), fx),
                    None => {
                        let (seen, snapshot_seq) =
                            s.snapshot_seen.remove(&scope).unwrap_or_default();
                        s.scopes.insert(
                            scope.clone(),
                            ScopeSync::SnapshotFinishing { req, snapshot_seq },
                        );
                        fx.push(Effect::FinishSnapshot {
                            scope,
                            seen,
                            snapshot_seq,
                            tag: ApplyTag::SnapshotFinish(req),
                        });
                    }
                }
            }
            (
                Some(ScopeSync::SnapshotFinishing { req, snapshot_seq }),
                ApplyTag::SnapshotFinish(t),
            ) if req == t => {
                s.cursors.insert(scope.clone(), snapshot_seq);
                self.request_changes(&scope, snapshot_seq, fx);
            }
            _ => {} // push applies and stale tags
        }
        self.request_pump(fx);
    }

    fn finish_scope(&mut self, scope: &str, fx: &mut Vec<Effect>) {
        let owes_recheck = {
            let Some(s) = self.session() else { return };
            s.recheck_before_live.remove(scope)
        };
        if owes_recheck {
            // An unreadable push arrived before this page sealed; it may be the change that
            // page missed, so check once more instead of declaring the scope live.
            self.start_catch_up(scope, fx);
            return;
        }
        let Some(s) = self.session() else { return };
        s.scopes.insert(scope.to_string(), ScopeSync::Live);
        s.catch_up_failures.remove(scope);
        let mut buffered = s.buffered.remove(scope).unwrap_or_default();
        buffered.sort_by_key(|c| c.seq);
        for change in buffered {
            self.on_push(change, fx);
            let still_live = self
                .session()
                .is_some_and(|s| s.scopes.get(scope) == Some(&ScopeSync::Live));
            if !still_live {
                break; // a gap restarted catch-up; it will cover the rest
            }
        }
        self.check_all_live(fx);
    }

    fn check_all_live(&mut self, fx: &mut Vec<Effect>) {
        let names = self.scope_names.clone();
        let Some(s) = self.session() else { return };
        if s.phase != Some(Phase::CatchingUp) {
            return;
        }
        if names.iter().all(|n| {
            matches!(
                s.scopes.get(n),
                Some(ScopeSync::Live) | Some(ScopeSync::Dropped)
            )
        }) {
            s.phase = Some(Phase::Live);
            if !s.synced_once {
                s.synced_once = true;
                fx.push(Effect::Emit(Lifecycle::SyncCompleted));
            }
        }
    }

    fn request_pump(&mut self, fx: &mut Vec<Effect>) {
        let Some(s) = self.session() else { return };
        fx.push(Effect::Cancel(TimerId::Pump));
        fx.push(Effect::Schedule {
            timer: TimerId::Pump,
            after: QUIET,
        });
        if !s.pump_scheduled {
            s.pump_scheduled = true;
            fx.push(Effect::Schedule {
                timer: TimerId::PumpCap,
                after: QUIET_CAP,
            });
        }
    }

    fn try_build(&mut self, doc_id: Uuid, fx: &mut Vec<Effect>) {
        let Some(s) = self.session() else { return };
        let busy = s.in_flight.contains_key(&doc_id)
            || s.building.contains(&doc_id)
            || s.backing_off.contains(&doc_id)
            || s.fetching.contains(&doc_id)
            || s.settling.contains(&doc_id);
        if busy {
            return;
        }
        if s.slots_used() >= MAX_IN_FLIGHT {
            s.pump_deferred = true;
            return;
        }
        s.building.insert(doc_id);
        fx.push(Effect::BuildUpload { doc_id });
    }

    /// Reissues `LoadPending` once a slot frees after `try_build` refused a doc purely for
    /// being at capacity.
    fn refill(&mut self, fx: &mut Vec<Effect>) {
        let Some(s) = self.session() else { return };
        if s.pump_deferred && s.slots_used() < MAX_IN_FLIGHT {
            s.pump_deferred = false;
            fx.push(Effect::LoadPending);
        }
    }

    fn back_off_doc(&mut self, doc_id: Uuid, after_ms: Option<u64>, fx: &mut Vec<Effect>) {
        let Some(s) = self.session() else { return };
        let failures = s.doc_failures.entry(doc_id).or_insert(0);
        *failures += 1;
        let delay = after_ms
            .map(Duration::from_millis)
            .unwrap_or_else(|| doc_retry_delay(*failures));
        s.backing_off.insert(doc_id);
        fx.push(Effect::Schedule {
            timer: TimerId::DocRetry(doc_id),
            after: delay,
        });
    }

    fn forget_failures(&mut self, doc_id: Uuid) {
        if let Some(s) = self.session() {
            s.mismatch_attempts.remove(&doc_id);
            s.doc_failures.remove(&doc_id);
        }
    }

    fn send_get_document(&mut self, doc_id: Uuid, fx: &mut Vec<Effect>) {
        let req = self.alloc_req();
        let Some(s) = self.session() else { return };
        s.fetching.insert(doc_id);
        s.requests.insert(req, Pending::Document { doc_id });
        fx.push(Effect::Send {
            req,
            request: Request::GetDocument { doc_id },
        });
        fx.push(Effect::Schedule {
            timer: TimerId::Request(req),
            after: REQUEST_TIMEOUT,
        });
    }

    fn on_upload_input(&mut self, input: Input, fx: &mut Vec<Effect>) {
        match input {
            Input::OutboxChanged => self.request_pump(fx),
            Input::PendingDocs(docs) => {
                for d in docs {
                    self.try_build(d, fx);
                }
            }
            Input::UploadBuilt { doc_id, outcome } => {
                let was_building = self.session().is_some_and(|s| s.building.remove(&doc_id));
                if !was_building {
                    return;
                }
                match outcome {
                    BuildOutcome::Send { upload, inflight } => {
                        let req = self.alloc_req();
                        let Some(s) = self.session() else { return };
                        s.in_flight.insert(doc_id, inflight);
                        s.requests.insert(req, Pending::Upload { doc_id });
                        fx.push(Effect::Send {
                            req,
                            request: Request::Upload(upload),
                        });
                        fx.push(Effect::Schedule {
                            timer: TimerId::Request(req),
                            after: REQUEST_TIMEOUT,
                        });
                    }
                    BuildOutcome::NeedsServerCopy => self.send_get_document(doc_id, fx),
                    BuildOutcome::SettledLocally { rows_remain } => {
                        self.forget_failures(doc_id);
                        if rows_remain {
                            self.try_build(doc_id, fx);
                        }
                    }
                    BuildOutcome::Nothing => self.forget_failures(doc_id),
                }
                self.refill(fx);
            }
            Input::Settled { doc_id, outcome } => {
                if let Some(s) = self.session() {
                    s.settling.remove(&doc_id);
                }
                match outcome {
                    SettleOutcome::Done { rows_remain } => {
                        self.forget_failures(doc_id);
                        if rows_remain {
                            self.try_build(doc_id, fx);
                        }
                    }
                    SettleOutcome::FetchServerCopy => {
                        if let Some(s) = self.session() {
                            *s.mismatch_attempts.entry(doc_id).or_insert(0) += 1;
                        }
                        self.send_get_document(doc_id, fx);
                    }
                    SettleOutcome::Retry {
                        after_ms,
                        mismatch: true,
                    } => {
                        if let Some(s) = self.session() {
                            *s.mismatch_attempts.entry(doc_id).or_insert(0) += 1;
                        }
                        match after_ms {
                            Some(ms) => self.back_off_doc(doc_id, Some(ms), fx),
                            None => self.try_build(doc_id, fx),
                        }
                    }
                    SettleOutcome::Retry {
                        after_ms,
                        mismatch: false,
                    } => self.back_off_doc(doc_id, after_ms, fx),
                }
                self.refill(fx);
            }
            Input::ServerCopyApplied { doc_id } => {
                if let Some(s) = self.session() {
                    s.fetching.remove(&doc_id);
                }
                self.try_build(doc_id, fx);
                self.refill(fx);
            }
            _ => {}
        }
    }

    fn on_upload_timer(&mut self, timer: TimerId, fx: &mut Vec<Effect>) {
        match timer {
            TimerId::Pump | TimerId::PumpCap => {
                let Some(s) = self.session() else { return };
                if !s.pump_scheduled {
                    return;
                }
                s.pump_scheduled = false;
                fx.push(Effect::Cancel(if timer == TimerId::Pump {
                    TimerId::PumpCap
                } else {
                    TimerId::Pump
                }));
                fx.push(Effect::LoadPending);
            }
            TimerId::DocRetry(doc_id) => {
                let was_waiting = self
                    .session()
                    .is_some_and(|s| s.backing_off.remove(&doc_id));
                if was_waiting {
                    self.try_build(doc_id, fx);
                }
            }
            _ => {}
        }
    }

    fn on_request_timeout(&mut self, pending: Pending, fx: &mut Vec<Effect>) {
        match pending {
            Pending::Upload { doc_id } => {
                if let Some(s) = self.session() {
                    s.in_flight.remove(&doc_id);
                }
                self.back_off_doc(doc_id, None, fx);
            }
            Pending::Document { doc_id } => {
                if let Some(s) = self.session() {
                    s.fetching.remove(&doc_id);
                }
                self.back_off_doc(doc_id, None, fx);
            }
            _ => {}
        }
        self.refill(fx);
    }
}

#[cfg(test)]
pub(crate) mod harness {
    use super::*;

    pub const ME: Uuid = Uuid::from_u128(0xA);

    pub fn core() -> Core {
        Core::new(vec!["own".into(), "collection:curated".into()], 7)
    }

    /// The driver's answer for the socket the core most recently opened.
    pub fn opened(c: &Core) -> Input {
        Input::SocketOpened { gen: c.socket_gen }
    }

    pub fn closed(c: &Core) -> Input {
        Input::SocketClosed { gen: c.socket_gen }
    }

    pub fn opens(fx: &[Effect]) -> bool {
        fx.iter().any(|e| matches!(e, Effect::OpenSocket { .. }))
    }

    pub fn closes(fx: &[Effect]) -> bool {
        fx.iter().any(|e| matches!(e, Effect::CloseSocket { .. }))
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

    pub fn scope_is_live(c: &mut Core, scope: &str) -> bool {
        c.session()
            .is_some_and(|s| s.scopes.get(scope) == Some(&ScopeSync::Live))
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
        assert!(c.step(closed(&c)).is_empty());
    }
}

#[cfg(test)]
mod connection_tests {
    use super::harness::*;
    use super::*;

    fn open_and_join(c: &mut Core) -> Vec<Effect> {
        let fx = c.step(opened(c));
        let (req, request) = sends(&fx).pop().expect("join sent");
        assert_eq!(request, Request::Join);
        c.step(Input::Reply {
            req,
            result: Ok(Response::Joined),
        })
    }

    #[test]
    fn stale_socket_closed_is_ignored() {
        let mut c = core();
        let fx = c.step(Input::Start {
            has_credentials: true,
        });
        assert!(fx.contains(&Effect::OpenSocket { gen: 1 }));
        let fx = c.step(Input::Timer(TimerId::ConnectTimeout));
        assert!(fx.contains(&Effect::CloseSocket { gen: 1 }));
        let fx = c.step(Input::Timer(TimerId::Reconnect));
        assert!(fx.contains(&Effect::OpenSocket { gen: 2 }));
        assert!(c.step(Input::SocketClosed { gen: 1 }).is_empty());
        assert!(c.step(Input::SocketOpened { gen: 1 }).is_empty());
        assert_eq!(c.state().connection, ConnectionView::Connecting);
        let fx = c.step(Input::SocketOpened { gen: 2 });
        assert_eq!(sends(&fx).pop().map(|(_, r)| r), Some(Request::Join));
    }

    fn update_required() -> ServerError {
        let mut error = ServerError::new("update_required");
        error.is_fatal = true;
        error
    }

    #[test]
    fn connect_refused_update_required_halts_without_retry() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        let fx = c.step(Input::ConnectRefused {
            gen: c.socket_gen,
            error: update_required(),
        });
        assert_eq!(
            c.state().connection,
            ConnectionView::Halted(HaltReason::UpdateRequired)
        );
        assert!(fx.contains(&Effect::Cancel(TimerId::ConnectTimeout)));
        assert!(!fx.iter().any(|e| matches!(
            e,
            Effect::Schedule {
                timer: TimerId::HaltRetry | TimerId::Reconnect,
                ..
            }
        )));
        assert!(emitted(&fx).iter().any(|l| matches!(
            l,
            Lifecycle::SyncError { fatal: true, code, .. } if code == "update_required"
        )));
    }

    #[test]
    fn connect_refused_transient_backs_off_with_retry_after() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        let mut error = ServerError::new("rate_limited");
        error.retry_after_ms = Some(4_000);
        let fx = c.step(Input::ConnectRefused {
            gen: c.socket_gen,
            error,
        });
        assert_eq!(c.state().connection, ConnectionView::Disconnected);
        assert!(fx.contains(&Effect::Schedule {
            timer: TimerId::Reconnect,
            after: Duration::from_millis(4_000)
        }));
    }

    #[test]
    fn stale_connect_refused_is_ignored() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        c.step(Input::Timer(TimerId::ConnectTimeout));
        c.step(Input::Timer(TimerId::Reconnect));
        assert!(c
            .step(Input::ConnectRefused {
                gen: 1,
                error: update_required(),
            })
            .is_empty());
        assert_eq!(c.state().connection, ConnectionView::Connecting);
    }

    #[test]
    fn start_without_credentials_halts_not_enrolled_with_retry() {
        let mut c = core();
        let fx = c.step(Input::Start {
            has_credentials: false,
        });
        assert_eq!(
            c.state().connection,
            ConnectionView::Halted(HaltReason::NotEnrolled)
        );
        assert!(fx.contains(&Effect::Schedule {
            timer: TimerId::HaltRetry,
            after: Duration::from_secs(300)
        }));
        assert!(emitted(&fx)
            .iter()
            .any(|l| matches!(l, Lifecycle::SyncError { fatal: true, .. })));
    }

    #[test]
    fn start_connects_and_emits_attempted_once() {
        let mut c = core();
        let fx = c.step(Input::Start {
            has_credentials: true,
        });
        assert!(opens(&fx));
        assert_eq!(emitted(&fx), vec![Lifecycle::ConnectionAttempted]);
        assert_eq!(c.state().connection, ConnectionView::Connecting);
    }

    #[test]
    fn join_ok_connects_and_loads_cursors() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        let fx = open_and_join(&mut c);
        assert_eq!(emitted(&fx), vec![Lifecycle::ConnectionSucceeded]);
        assert!(fx.contains(&Effect::LoadCursors));
        assert!(fx.contains(&Effect::Schedule {
            timer: TimerId::Heartbeat,
            after: Duration::from_secs(30)
        }));
        assert!(fx.contains(&Effect::Schedule {
            timer: TimerId::StableReset,
            after: Duration::from_secs(60)
        }));
    }

    #[test]
    fn transient_join_error_backs_off_and_reconnects() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        let fx = c.step(opened(&c));
        let (req, _) = sends(&fx).pop().unwrap();
        let fx = c.step(Input::Reply {
            req,
            result: Err(ServerError::new("internal")),
        });
        assert_eq!(c.state().connection, ConnectionView::Disconnected);
        assert!(fx.iter().any(|e| matches!(e, Effect::Schedule { timer: TimerId::Reconnect, after } if *after == Duration::from_secs(1))));
        let fx = c.step(Input::Timer(TimerId::Reconnect));
        assert!(opens(&fx));
    }

    #[test]
    fn clock_skew_join_error_backs_off_and_reports_without_halting() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        let (req, _) = sends(&c.step(opened(&c))).pop().unwrap();
        let mut skew = ServerError::new("clock_skew");
        skew.server_time = Some(1_767_225_600);
        let fx = c.step(Input::Reply {
            req,
            result: Err(skew),
        });
        assert_eq!(c.state().connection, ConnectionView::Disconnected);
        assert!(closes(&fx));
        assert!(fx.iter().any(|e| matches!(
            e,
            Effect::Schedule {
                timer: TimerId::Reconnect,
                ..
            }
        )));
        assert_eq!(
            emitted(&fx),
            vec![Lifecycle::SyncError {
                code: "clock_skew".into(),
                scope: None,
                doc_id: None,
                fatal: false
            }]
        );
    }

    #[test]
    fn server_retry_after_overrides_backoff() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        let (req, _) = sends(&c.step(opened(&c))).pop().unwrap();
        let err = ServerError {
            retry_after_ms: Some(12_000),
            ..ServerError::new("rate_limited")
        };
        let fx = c.step(Input::Reply {
            req,
            result: Err(err),
        });
        assert!(fx.contains(&Effect::Schedule {
            timer: TimerId::Reconnect,
            after: Duration::from_millis(12_000)
        }));
    }

    #[test]
    fn update_required_halts_without_retry_timer() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        let (req, _) = sends(&c.step(opened(&c))).pop().unwrap();
        let err = ServerError {
            is_fatal: true,
            ..ServerError::new("update_required")
        };
        let fx = c.step(Input::Reply {
            req,
            result: Err(err),
        });
        assert_eq!(
            c.state().connection,
            ConnectionView::Halted(HaltReason::UpdateRequired)
        );
        assert!(!fx.iter().any(|e| matches!(
            e,
            Effect::Schedule {
                timer: TimerId::HaltRetry,
                ..
            }
        )));
        assert!(closes(&fx));
    }

    #[test]
    fn auth_invalid_retries_via_credential_check() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        let (req, _) = sends(&c.step(opened(&c))).pop().unwrap();
        c.step(Input::Reply {
            req,
            result: Err(ServerError {
                is_fatal: true,
                ..ServerError::new("auth_invalid")
            }),
        });
        let fx = c.step(Input::Timer(TimerId::HaltRetry));
        assert_eq!(fx.first(), Some(&Effect::CheckCredentials));
        let fx = c.step(Input::CredentialsChanged {
            has_credentials: true,
        });
        assert!(opens(&fx));
    }

    #[test]
    fn unknown_code_follows_is_fatal() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        let (req, _) = sends(&c.step(opened(&c))).pop().unwrap();
        c.step(Input::Reply {
            req,
            result: Err(ServerError {
                is_fatal: true,
                ..ServerError::new("weird")
            }),
        });
        assert_eq!(
            c.state().connection,
            ConnectionView::Halted(HaltReason::Other("weird".into()))
        );
    }

    #[test]
    fn connect_timeout_counts_as_transient_failure() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        let fx = c.step(Input::Timer(TimerId::ConnectTimeout));
        assert!(closes(&fx));
        assert_eq!(c.state().connection, ConnectionView::Disconnected);
    }

    #[test]
    fn socket_closed_while_connected_emits_connection_lost_once() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        open_and_join(&mut c);
        let fx = c.step(closed(&c));
        assert_eq!(emitted(&fx), vec![Lifecycle::ConnectionLost]);
        assert!(emitted(&c.step(closed(&c))).is_empty());
    }

    #[test]
    fn heartbeat_timeout_drops_connection() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        open_and_join(&mut c);
        let fx = c.step(Input::Timer(TimerId::Heartbeat));
        let (req, request) = sends(&fx).pop().unwrap();
        assert_eq!(request, Request::Heartbeat);
        let fx = c.step(Input::Timer(TimerId::HeartbeatTimeout(req)));
        assert_eq!(emitted(&fx), vec![Lifecycle::ConnectionLost]);
    }

    #[test]
    fn heartbeat_reply_cancels_timeout() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        open_and_join(&mut c);
        let (req, _) = sends(&c.step(Input::Timer(TimerId::Heartbeat)))
            .pop()
            .unwrap();
        let fx = c.step(Input::Reply {
            req,
            result: Ok(Response::HeartbeatOk),
        });
        assert!(fx.contains(&Effect::Cancel(TimerId::HeartbeatTimeout(req))));
    }

    #[test]
    fn stale_heartbeat_timeout_after_reconnect_is_ignored() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        open_and_join(&mut c);
        let (old_req, _) = sends(&c.step(Input::Timer(TimerId::Heartbeat)))
            .pop()
            .unwrap();
        c.step(closed(&c));
        c.step(Input::Reconnect);
        open_and_join(&mut c);
        let fx = c.step(Input::Timer(TimerId::HeartbeatTimeout(old_req)));
        assert!(emitted(&fx).is_empty());
        assert_eq!(c.state().connection, ConnectionView::Connected);
    }

    #[test]
    fn reconnect_is_noop_while_connecting_or_connected() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        assert!(!opens(&c.step(Input::Reconnect)));
        open_and_join(&mut c);
        assert!(!opens(&c.step(Input::Reconnect)));
    }

    #[test]
    fn stable_reset_zeroes_attempts() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        for _ in 0..5 {
            c.step(Input::Timer(TimerId::ConnectTimeout));
            c.step(Input::Timer(TimerId::Reconnect));
        }
        open_and_join(&mut c);
        c.step(Input::Timer(TimerId::StableReset));
        let fx = c.step(closed(&c));
        assert!(fx.iter().any(|e| matches!(e, Effect::Schedule { timer: TimerId::Reconnect, after } if *after == Duration::from_secs(1))));
    }

    #[test]
    fn reconnect_from_halted_resets_backoff() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        for _ in 0..5 {
            c.step(Input::Timer(TimerId::ConnectTimeout));
            c.step(Input::Timer(TimerId::Reconnect));
        }
        let (req, _) = sends(&c.step(opened(&c))).pop().unwrap();
        c.step(Input::Reply {
            req,
            result: Err(ServerError {
                is_fatal: true,
                ..ServerError::new("auth_invalid")
            }),
        });
        assert_eq!(
            c.state().connection,
            ConnectionView::Halted(HaltReason::AuthInvalid)
        );
        c.step(Input::Reconnect);
        let fx = c.step(Input::Timer(TimerId::ConnectTimeout));
        assert!(fx.iter().any(|e| matches!(e, Effect::Schedule { timer: TimerId::Reconnect, after } if *after == Duration::from_secs(1))));
    }

    #[test]
    fn shutdown_stops_everything() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        open_and_join(&mut c);
        let fx = c.step(Input::Shutdown);
        assert!(closes(&fx));
        assert_eq!(c.state().connection, ConnectionView::Stopped);
        assert!(c.step(Input::Reconnect).is_empty());
        assert!(c.step(Input::Timer(TimerId::Heartbeat)).is_empty());
    }

    #[test]
    fn shutdown_before_start_stops() {
        let mut c = core();
        c.step(Input::Shutdown);
        assert_eq!(c.state().connection, ConnectionView::Stopped);
        assert!(c
            .step(Input::Start {
                has_credentials: true
            })
            .is_empty());
    }

    #[test]
    fn retry_after_zero_still_waits_the_minimum() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        let mut rate_limited = ServerError::new("rate_limited");
        rate_limited.retry_after_ms = Some(0);
        let fx = c.step(Input::ConnectRefused {
            gen: 1,
            error: rate_limited,
        });
        assert!(fx.contains(&Effect::Schedule {
            timer: TimerId::Reconnect,
            after: Duration::from_secs(1),
        }));
    }
}

#[cfg(test)]
mod catch_up_tests {
    use super::harness::*;
    use super::*;
    use crate::engine::types::{ChangeKind, DocEnvelope};
    use serde_json::json;

    fn connected(c: &mut Core) {
        c.step(Input::Start {
            has_credentials: true,
        });
        let (req, _) = sends(&c.step(opened(c))).pop().unwrap();
        c.step(Input::Reply {
            req,
            result: Ok(Response::Joined),
        });
    }

    fn change(scope: &str, seq: i64, prev_seq: i64) -> Change {
        Change {
            scope: scope.into(),
            seq,
            prev_seq,
            doc_id: Uuid::from_u128(seq as u128),
            kind: ChangeKind::Upsert,
            doc: Some(DocEnvelope {
                doc_id: Uuid::from_u128(seq as u128),
                owner_id: None,
                author_id: None,
                read_only: false,
                source_doc_id: None,
                derived_from: None,
                title: None,
                content: json!({}),
                hash: "h".into(),
                seq,
            }),
            client_id: None,
            upload_id: None,
        }
    }

    fn changes_req(fx: &[Effect], scope: &str) -> u64 {
        sends(fx)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::GetChangesSince { scope: s, .. } if s == scope))
            .map(|(req, _)| req)
            .expect("changes request")
    }

    /// Connect with both scopes at a non-zero cursor and complete catch-up with empty pages.
    fn live(c: &mut Core) {
        connected(c);
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));
        for scope in ["own", "collection:curated"] {
            let req = changes_req(&fx, scope);
            c.step(Input::Reply {
                req,
                result: Ok(Response::Changes {
                    changes: vec![],
                    next_cursor: 5,
                    has_more: false,
                }),
            });
            c.step(Input::Applied {
                scope: scope.into(),
                tag: ApplyTag::Page(req),
            });
        }
        assert_eq!(c.state().sync, SyncView::Live);
    }

    #[test]
    fn cursors_start_catch_up_with_changes_since_and_sync_started() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 9),
        ]));
        assert_eq!(emitted(&fx), vec![Lifecycle::SyncStarted]);
        let reqs = sends(&fx);
        assert!(reqs.iter().any(|(_, r)| *r
            == Request::GetChangesSince {
                scope: "own".into(),
                cursor: 5,
                limit: 500
            }));
        assert!(reqs.iter().any(|(_, r)| *r
            == Request::GetChangesSince {
                scope: "collection:curated".into(),
                cursor: 9,
                limit: 500
            }));
    }

    #[test]
    fn zero_cursor_starts_snapshot() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![]));
        assert!(sends(&fx).iter().any(|(_, r)| *r
            == Request::GetSnapshot {
                scope: "own".into(),
                page_token: None
            }));
    }

    #[test]
    fn sync_completed_once_after_all_scopes_caught_up() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));
        let own = changes_req(&fx, "own");
        let cur = changes_req(&fx, "collection:curated");
        c.step(Input::Reply {
            req: own,
            result: Ok(Response::Changes {
                changes: vec![],
                next_cursor: 5,
                has_more: false,
            }),
        });
        let fx = c.step(Input::Applied {
            scope: "own".into(),
            tag: ApplyTag::Page(own),
        });
        assert!(emitted(&fx).is_empty());
        c.step(Input::Reply {
            req: cur,
            result: Ok(Response::Changes {
                changes: vec![],
                next_cursor: 5,
                has_more: false,
            }),
        });
        let fx = c.step(Input::Applied {
            scope: "collection:curated".into(),
            tag: ApplyTag::Page(cur),
        });
        assert_eq!(emitted(&fx), vec![Lifecycle::SyncCompleted]);
    }

    #[test]
    fn has_more_requests_next_page_from_next_cursor() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));
        let own = changes_req(&fx, "own");
        let fx = c.step(Input::Reply {
            req: own,
            result: Ok(Response::Changes {
                changes: vec![change("own", 6, 5)],
                next_cursor: 6,
                has_more: true,
            }),
        });
        assert!(fx.iter().any(|e| matches!(e, Effect::ApplyChanges { new_cursor: 6, tag: ApplyTag::Page(r), .. } if *r == own)));
        let fx = c.step(Input::Applied {
            scope: "own".into(),
            tag: ApplyTag::Page(own),
        });
        assert!(sends(&fx).iter().any(|(_, r)| *r
            == Request::GetChangesSince {
                scope: "own".into(),
                cursor: 6,
                limit: 500
            }));
    }

    #[test]
    fn cursor_too_old_switches_to_snapshot_then_catches_up_from_snapshot_seq() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));
        let own = changes_req(&fx, "own");
        let fx = c.step(Input::Reply {
            req: own,
            result: Err(ServerError::new("cursor_too_old")),
        });
        let (snap_req, _) = sends(&fx)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::GetSnapshot { .. }))
            .unwrap();
        c.step(Input::Reply {
            req: snap_req,
            result: Ok(Response::SnapshotPage {
                docs: vec![],
                snapshot_seq: 40,
                next_page_token: None,
            }),
        });
        let fx = c.step(Input::Applied {
            scope: "own".into(),
            tag: ApplyTag::SnapshotPage(snap_req),
        });
        assert!(fx.contains(&Effect::FinishSnapshot {
            scope: "own".into(),
            seen: vec![],
            snapshot_seq: 40,
            tag: ApplyTag::SnapshotFinish(snap_req),
        }));
        let fx = c.step(Input::Applied {
            scope: "own".into(),
            tag: ApplyTag::SnapshotFinish(snap_req),
        });
        assert!(sends(&fx).iter().any(|(_, r)| *r
            == Request::GetChangesSince {
                scope: "own".into(),
                cursor: 40,
                limit: 500
            }));
    }

    #[test]
    fn catch_up_failure_retries_then_drops_connection_after_three() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));
        let mut req = changes_req(&fx, "own");
        for n in 1..=2u64 {
            let fx = c.step(Input::Reply {
                req,
                result: Err(ServerError::new("internal")),
            });
            assert!(fx.contains(&Effect::Schedule {
                timer: TimerId::CatchUpRetry("own".into()),
                after: Duration::from_secs(1 << (n - 1))
            }));
            req = changes_req(
                &c.step(Input::Timer(TimerId::CatchUpRetry("own".into()))),
                "own",
            );
        }
        let fx = c.step(Input::Reply {
            req,
            result: Err(ServerError::new("internal")),
        });
        assert_eq!(emitted(&fx), vec![Lifecycle::ConnectionLost]);
    }

    #[test]
    fn live_push_applies_in_order_and_skips_seen() {
        let mut c = core();
        live(&mut c);
        let fx = c.step(Input::Push(change("own", 6, 5)));
        assert!(fx.iter().any(|e| matches!(
            e,
            Effect::ApplyChanges {
                new_cursor: 6,
                tag: ApplyTag::Push,
                ..
            }
        )));
        assert!(c
            .step(Input::Push(change("own", 6, 5)))
            .iter()
            .all(|e| !matches!(e, Effect::ApplyChanges { .. })));
    }

    #[test]
    fn push_with_prev_seq_below_cursor_applies() {
        // After a snapshot the cursor is a global position; the next push's prev_seq is lower.
        let mut c = core();
        live(&mut c);
        let fx = c.step(Input::Push(change("own", 9, 2)));
        assert!(fx
            .iter()
            .any(|e| matches!(e, Effect::ApplyChanges { new_cursor: 9, .. })));
    }

    #[test]
    fn push_gap_starts_catch_up_without_new_sync_events() {
        let mut c = core();
        live(&mut c);
        let fx = c.step(Input::Push(change("own", 12, 11)));
        assert!(sends(&fx).iter().any(|(_, r)| *r
            == Request::GetChangesSince {
                scope: "own".into(),
                cursor: 5,
                limit: 500
            }));
        assert!(emitted(&fx).is_empty());
    }

    #[test]
    fn unreadable_push_starts_catch_up_for_its_scope() {
        let mut c = core();
        live(&mut c);
        let fx = c.step(Input::UnreadablePush {
            scope: Some("own".into()),
        });
        assert_eq!(
            sends(&fx),
            vec![(
                changes_req(&fx, "own"),
                Request::GetChangesSince {
                    scope: "own".into(),
                    cursor: 5,
                    limit: PAGE_LIMIT
                }
            )]
        );
        assert!(emitted(&fx).is_empty());
    }

    #[test]
    fn unreadable_push_without_a_scope_catches_up_every_live_scope() {
        let mut c = core();
        live(&mut c);
        let fx = c.step(Input::UnreadablePush { scope: None });
        changes_req(&fx, "own");
        changes_req(&fx, "collection:curated");
    }

    #[test]
    fn third_unreadable_push_on_a_scope_emits_one_sync_error() {
        let mut c = core();
        live(&mut c);
        for push in 1..=5 {
            let fx = c.step(Input::UnreadablePush {
                scope: Some("own".into()),
            });
            let errors = emitted(&fx);
            if push == 3 {
                assert_eq!(
                    errors,
                    vec![Lifecycle::SyncError {
                        code: "protocol_error".into(),
                        scope: Some("own".into()),
                        doc_id: None,
                        fatal: false
                    }]
                );
            } else {
                assert!(errors.is_empty(), "push {push}: {errors:?}");
            }
        }
    }

    #[test]
    fn unreadable_push_before_the_final_page_defers_going_live() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));
        let own = changes_req(&fx, "own");
        c.step(Input::Reply {
            req: own,
            result: Ok(Response::Changes {
                changes: vec![],
                next_cursor: 5,
                has_more: false,
            }),
        });
        // The push arrives while the page is still `Applying`, before it seals the scope live.
        let fx = c.step(Input::UnreadablePush {
            scope: Some("own".into()),
        });
        assert!(sends(&fx).is_empty());
        let fx = c.step(Input::Applied {
            scope: "own".into(),
            tag: ApplyTag::Page(own),
        });
        assert!(sends(&fx).iter().any(|(_, r)| *r
            == Request::GetChangesSince {
                scope: "own".into(),
                cursor: 5,
                limit: PAGE_LIMIT
            }));
        assert!(!scope_is_live(&mut c, "own"));
    }

    #[test]
    fn unreadable_push_during_snapshot_finishing_defers_going_live() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 0),
            ("collection:curated".into(), 5),
        ]));
        let (snap_req, _) = sends(&fx)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::GetSnapshot { .. }))
            .unwrap();
        c.step(Input::Reply {
            req: snap_req,
            result: Ok(Response::SnapshotPage {
                docs: vec![],
                snapshot_seq: 40,
                next_page_token: None,
            }),
        });
        c.step(Input::Applied {
            scope: "own".into(),
            tag: ApplyTag::SnapshotPage(snap_req),
        });
        // The push arrives while the scope is `SnapshotFinishing`, before it reaches a page
        // that could seal it live.
        c.step(Input::UnreadablePush {
            scope: Some("own".into()),
        });
        let fx = c.step(Input::Applied {
            scope: "own".into(),
            tag: ApplyTag::SnapshotFinish(snap_req),
        });
        let changes_req_id = sends(&fx)
            .into_iter()
            .find(|(_, r)| {
                *r == Request::GetChangesSince {
                    scope: "own".into(),
                    cursor: 40,
                    limit: PAGE_LIMIT,
                }
            })
            .map(|(req, _)| req)
            .expect("catch-up from the snapshot sequence");
        // That page is the first one reached since the push arrived; the mark it left on the
        // scope forces one more round instead of sealing the scope live here.
        c.step(Input::Reply {
            req: changes_req_id,
            result: Ok(Response::Changes {
                changes: vec![],
                next_cursor: 40,
                has_more: false,
            }),
        });
        let fx = c.step(Input::Applied {
            scope: "own".into(),
            tag: ApplyTag::Page(changes_req_id),
        });
        assert!(sends(&fx).iter().any(|(_, r)| *r
            == Request::GetChangesSince {
                scope: "own".into(),
                cursor: 40,
                limit: PAGE_LIMIT
            }));
        assert!(!scope_is_live(&mut c, "own"));
    }

    #[test]
    fn recheck_that_finds_nothing_then_goes_live() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));
        let own = changes_req(&fx, "own");
        c.step(Input::Reply {
            req: own,
            result: Ok(Response::Changes {
                changes: vec![],
                next_cursor: 5,
                has_more: false,
            }),
        });
        c.step(Input::UnreadablePush {
            scope: Some("own".into()),
        });
        let fx = c.step(Input::Applied {
            scope: "own".into(),
            tag: ApplyTag::Page(own),
        });
        let recheck_req = changes_req(&fx, "own");
        assert!(!scope_is_live(&mut c, "own"));
        c.step(Input::Reply {
            req: recheck_req,
            result: Ok(Response::Changes {
                changes: vec![],
                next_cursor: 5,
                has_more: false,
            }),
        });
        let fx = c.step(Input::Applied {
            scope: "own".into(),
            tag: ApplyTag::Page(recheck_req),
        });
        assert!(sends(&fx).is_empty());
        assert!(scope_is_live(&mut c, "own"));
    }

    #[test]
    fn push_during_catch_up_is_buffered_and_applied() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));
        let own = changes_req(&fx, "own");
        let fx = c.step(Input::Push(change("own", 8, 7)));
        assert!(!fx.iter().any(|e| matches!(e, Effect::ApplyChanges { .. })));
        c.step(Input::Reply {
            req: own,
            result: Ok(Response::Changes {
                changes: vec![],
                next_cursor: 7,
                has_more: false,
            }),
        });
        let fx = c.step(Input::Applied {
            scope: "own".into(),
            tag: ApplyTag::Page(own),
        });
        assert!(fx.iter().any(|e| matches!(
            e,
            Effect::ApplyChanges {
                new_cursor: 8,
                tag: ApplyTag::Push,
                ..
            }
        )));
    }

    #[test]
    fn late_push_applied_does_not_advance_catch_up() {
        let mut c = core();
        live(&mut c);
        c.step(Input::Push(change("own", 6, 5)));
        c.step(Input::Push(change("own", 12, 11))); // gap → catch-up requesting
        let fx = c.step(Input::Applied {
            scope: "own".into(),
            tag: ApplyTag::Push,
        });
        assert!(sends(&fx).is_empty());
    }

    #[test]
    fn reconnect_emits_new_sync_pair() {
        let mut c = core();
        live(&mut c);
        c.step(closed(&c));
        c.step(Input::Timer(TimerId::Reconnect));
        let (req, _) = sends(&c.step(opened(&c))).pop().unwrap();
        c.step(Input::Reply {
            req,
            result: Ok(Response::Joined),
        });
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));
        assert_eq!(emitted(&fx), vec![Lifecycle::SyncStarted]);
    }

    #[test]
    fn empty_scope_snapshot_goes_live() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![]));
        let snap_reqs: Vec<(String, u64)> = ["own", "collection:curated"]
            .iter()
            .map(|scope| {
                let (req, _) = sends(&fx)
                    .into_iter()
                    .find(
                        |(_, r)| matches!(r, Request::GetSnapshot { scope: s, .. } if s == *scope),
                    )
                    .unwrap();
                (scope.to_string(), req)
            })
            .collect();
        for (scope, snap_req) in snap_reqs {
            c.step(Input::Reply {
                req: snap_req,
                result: Ok(Response::SnapshotPage {
                    docs: vec![],
                    snapshot_seq: 0,
                    next_page_token: None,
                }),
            });
            let fx = c.step(Input::Applied {
                scope: scope.clone(),
                tag: ApplyTag::SnapshotPage(snap_req),
            });
            assert!(fx.contains(&Effect::FinishSnapshot {
                scope: scope.clone(),
                seen: vec![],
                snapshot_seq: 0,
                tag: ApplyTag::SnapshotFinish(snap_req),
            }));
            let fx = c.step(Input::Applied {
                scope: scope.clone(),
                tag: ApplyTag::SnapshotFinish(snap_req),
            });
            assert!(sends(&fx).iter().any(|(_, r)| *r
                == Request::GetChangesSince {
                    scope: scope.clone(),
                    cursor: 0,
                    limit: 500,
                }));
            let page_req = changes_req(&fx, &scope);
            c.step(Input::Reply {
                req: page_req,
                result: Ok(Response::Changes {
                    changes: vec![],
                    next_cursor: 0,
                    has_more: false,
                }),
            });
            c.step(Input::Applied {
                scope: scope.clone(),
                tag: ApplyTag::Page(page_req),
            });
        }
        assert_eq!(c.state().sync, SyncView::Live);
    }

    #[test]
    fn stale_snapshot_finish_is_ignored() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![("collection:curated".into(), 4)]));
        let (req, _) = sends(&fx)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::GetSnapshot { .. }))
            .unwrap();
        c.step(Input::Reply {
            req,
            result: Ok(Response::SnapshotPage {
                docs: vec![],
                snapshot_seq: 6,
                next_page_token: None,
            }),
        });
        let fx = c.step(Input::Applied {
            scope: "own".into(),
            tag: ApplyTag::SnapshotPage(req),
        });
        assert!(fx.contains(&Effect::FinishSnapshot {
            scope: "own".into(),
            seen: vec![],
            snapshot_seq: 6,
            tag: ApplyTag::SnapshotFinish(req),
        }));
        let fx = c.step(Input::Applied {
            scope: "own".into(),
            tag: ApplyTag::SnapshotFinish(req + 1000),
        });
        assert!(sends(&fx).is_empty());
        let fx = c.step(Input::Applied {
            scope: "own".into(),
            tag: ApplyTag::SnapshotFinish(req),
        });
        assert!(changes_req(&fx, "own") > req);
    }

    #[test]
    fn failures_in_different_scopes_count_separately() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));
        let own = changes_req(&fx, "own");
        let cur = changes_req(&fx, "collection:curated");
        let fx = c.step(Input::Reply {
            req: own,
            result: Err(ServerError::new("internal")),
        });
        assert!(fx.contains(&Effect::Schedule {
            timer: TimerId::CatchUpRetry("own".into()),
            after: Duration::from_secs(1),
        }));
        let fx = c.step(Input::Reply {
            req: cur,
            result: Err(ServerError::new("internal")),
        });
        assert!(fx.contains(&Effect::Schedule {
            timer: TimerId::CatchUpRetry("collection:curated".into()),
            after: Duration::from_secs(1),
        }));
    }

    #[test]
    fn catch_up_honours_retry_after() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));
        let own = changes_req(&fx, "own");
        let err = ServerError {
            retry_after_ms: Some(7_000),
            ..ServerError::new("rate_limited")
        };
        let fx = c.step(Input::Reply {
            req: own,
            result: Err(err),
        });
        assert!(fx.contains(&Effect::Schedule {
            timer: TimerId::CatchUpRetry("own".into()),
            after: Duration::from_secs(7),
        }));
    }

    #[test]
    fn fatal_catch_up_error_halts() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));
        let own = changes_req(&fx, "own");
        let err = ServerError {
            is_fatal: true,
            ..ServerError::new("auth_invalid")
        };
        let fx = c.step(Input::Reply {
            req: own,
            result: Err(err),
        });
        let em = emitted(&fx);
        assert_eq!(em.first(), Some(&Lifecycle::ConnectionLost));
        assert!(em
            .iter()
            .any(|l| matches!(l, Lifecycle::SyncError { fatal: true, .. })));
        assert_eq!(
            c.state().connection,
            ConnectionView::Halted(HaltReason::AuthInvalid)
        );
    }

    #[test]
    fn subscription_forbidden_drops_scope() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));
        let own = changes_req(&fx, "own");
        let cur = changes_req(&fx, "collection:curated");
        let fx = c.step(Input::Reply {
            req: cur,
            result: Err(ServerError::new("subscription_forbidden")),
        });
        assert!(fx.contains(&Effect::DropSubscription {
            scope: "collection:curated".into(),
        }));
        c.step(Input::Reply {
            req: own,
            result: Ok(Response::Changes {
                changes: vec![],
                next_cursor: 5,
                has_more: false,
            }),
        });
        let fx = c.step(Input::Applied {
            scope: "own".into(),
            tag: ApplyTag::Page(own),
        });
        assert_eq!(emitted(&fx), vec![Lifecycle::SyncCompleted]);
        let fx = c.step(Input::Push(change("collection:curated", 6, 5)));
        assert!(!fx.iter().any(|e| matches!(e, Effect::ApplyChanges { .. })));
    }

    #[test]
    fn dropped_scope_is_not_caught_up_next_connection() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));
        let curated = changes_req(&fx, "collection:curated");
        c.step(Input::Reply {
            req: curated,
            result: Err(ServerError::new("subscription_forbidden")),
        });

        c.step(closed(&c));
        c.step(Input::Timer(TimerId::Reconnect));
        let (req, _) = sends(&c.step(opened(&c))).pop().unwrap();
        c.step(Input::Reply {
            req,
            result: Ok(Response::Joined),
        });
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));

        let requested: Vec<String> = sends(&fx)
            .into_iter()
            .filter_map(|(_, request)| match request {
                Request::GetChangesSince { scope, .. } | Request::GetSnapshot { scope, .. } => {
                    Some(scope)
                }
                _ => None,
            })
            .collect();
        assert_eq!(requested, vec!["own".to_string()]);
    }
}

#[cfg(test)]
mod upload_orchestration_tests {
    use super::harness::*;
    use super::*;
    use crate::engine::doc::InFlight;
    use crate::engine::types::{DocEnvelope, UploadKind};
    use serde_json::json;

    fn doc(n: u128) -> Uuid {
        Uuid::from_u128(0x1000 + n)
    }

    fn connected(c: &mut Core) {
        c.step(Input::Start {
            has_credentials: true,
        });
        let (req, _) = sends(&c.step(opened(c))).pop().unwrap();
        c.step(Input::Reply {
            req,
            result: Ok(Response::Joined),
        });
    }

    fn prepared(d: Uuid) -> BuildOutcome {
        let upload_id = Uuid::from_u128(0x77);
        BuildOutcome::Send {
            upload: Upload {
                upload_id,
                doc_id: d,
                kind: UploadKind::Update,
                base_hash: Some("h".into()),
                payload: json!([]),
            },
            inflight: InFlight {
                upload_id,
                covered: vec![upload_id],
                kind: UploadKind::Update,
                base_hash: Some("h".into()),
                sent_content: json!({}),
            },
        }
    }

    fn envelope(d: Uuid, seq: i64) -> DocEnvelope {
        DocEnvelope {
            doc_id: d,
            owner_id: Some(ME),
            author_id: None,
            read_only: false,
            source_doc_id: None,
            derived_from: None,
            title: None,
            content: json!({}),
            hash: "h2".into(),
            seq,
        }
    }

    fn upload_req(fx: &[Effect]) -> u64 {
        sends(fx)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::Upload(_)))
            .map(|(q, _)| q)
            .expect("upload sent")
    }

    #[test]
    fn outbox_change_debounces_then_loads_pending() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));
        let fx = c.step(Input::OutboxChanged);
        assert!(fx.contains(&Effect::Cancel(TimerId::Pump)));
        assert!(fx.contains(&Effect::Schedule {
            timer: TimerId::Pump,
            after: Duration::from_millis(200)
        }));
        let fx = c.step(Input::Timer(TimerId::Pump));
        assert!(fx.contains(&Effect::LoadPending));
        assert!(fx.contains(&Effect::Cancel(TimerId::PumpCap)));
    }

    #[test]
    fn outbox_change_while_disconnected_does_nothing() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        assert!(!c.step(Input::OutboxChanged).iter().any(|e| matches!(
            e,
            Effect::Schedule {
                timer: TimerId::Pump,
                ..
            }
        )));
    }

    #[test]
    fn pending_docs_build_at_most_eight() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::PendingDocs((0..12).map(doc).collect()));
        let builds = fx
            .iter()
            .filter(|e| matches!(e, Effect::BuildUpload { .. }))
            .count();
        assert_eq!(builds, MAX_IN_FLIGHT);
    }

    #[test]
    fn built_upload_is_sent_with_timeout_and_settled_on_reply() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::PendingDocs(vec![doc(1)]));
        let fx = c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: prepared(doc(1)),
        });
        let req = upload_req(&fx);
        assert!(fx.contains(&Effect::Schedule {
            timer: TimerId::Request(req),
            after: Duration::from_secs(30)
        }));
        let fx = c.step(Input::Reply {
            req,
            result: Ok(Response::Uploaded(envelope(doc(1), 9))),
        });
        assert!(fx.iter().any(|e| matches!(e, Effect::SettleUpload { doc_id, reply: Ok(_), mismatch_attempts: 0, .. } if *doc_id == doc(1))));
    }

    #[test]
    fn document_already_in_flight_is_not_rebuilt() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::PendingDocs(vec![doc(1)]));
        c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: prepared(doc(1)),
        });
        let fx = c.step(Input::PendingDocs(vec![doc(1)]));
        assert!(!fx.iter().any(|e| matches!(e, Effect::BuildUpload { .. })));
    }

    #[test]
    fn refused_docs_are_refilled_when_a_slot_frees() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::PendingDocs((0..12).map(doc).collect()));
        let builds = fx
            .iter()
            .filter(|e| matches!(e, Effect::BuildUpload { .. }))
            .count();
        assert_eq!(builds, MAX_IN_FLIGHT);
        let fx = c.step(Input::UploadBuilt {
            doc_id: doc(0),
            outcome: BuildOutcome::Nothing,
        });
        assert!(fx.contains(&Effect::LoadPending));
    }

    #[test]
    fn doc_is_not_rebuilt_while_fetching_server_copy() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::PendingDocs(vec![doc(1)]));
        let fx = c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: BuildOutcome::NeedsServerCopy,
        });
        let (req, _) = sends(&fx).pop().unwrap();
        let fx = c.step(Input::PendingDocs(vec![doc(1)]));
        assert!(!fx.iter().any(|e| matches!(e, Effect::BuildUpload { .. })));
        c.step(Input::Reply {
            req,
            result: Ok(Response::Document(envelope(doc(1), 3))),
        });
        let fx = c.step(Input::ServerCopyApplied { doc_id: doc(1) });
        assert!(fx.contains(&Effect::BuildUpload { doc_id: doc(1) }));
    }

    #[test]
    fn stale_upload_built_is_ignored() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: prepared(doc(1)),
        });
        assert!(sends(&fx).is_empty());
    }

    #[test]
    fn settled_with_rows_remaining_builds_again() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Settled {
            doc_id: doc(1),
            outcome: SettleOutcome::Done { rows_remain: true },
        });
        assert!(fx.contains(&Effect::BuildUpload { doc_id: doc(1) }));
    }

    #[test]
    fn needs_server_copy_fetches_then_rebuilds() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::PendingDocs(vec![doc(1)]));
        let fx = c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: BuildOutcome::NeedsServerCopy,
        });
        let (req, request) = sends(&fx).pop().unwrap();
        assert_eq!(request, Request::GetDocument { doc_id: doc(1) });
        let fx = c.step(Input::Reply {
            req,
            result: Ok(Response::Document(envelope(doc(1), 3))),
        });
        assert!(fx
            .iter()
            .any(|e| matches!(e, Effect::ApplyServerCopy { doc: Some(_), .. })));
        let fx = c.step(Input::ServerCopyApplied { doc_id: doc(1) });
        assert!(fx.contains(&Effect::BuildUpload { doc_id: doc(1) }));
    }

    #[test]
    fn get_document_not_found_applies_missing() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::PendingDocs(vec![doc(1)]));
        let (req, _) = sends(&c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: BuildOutcome::NeedsServerCopy,
        }))
        .pop()
        .unwrap();
        let fx = c.step(Input::Reply {
            req,
            result: Err(ServerError::new("not_found")),
        });
        assert!(fx.contains(&Effect::ApplyServerCopy {
            doc_id: doc(1),
            doc: None
        }));
    }

    #[test]
    fn get_document_deleted_applies_server_deleted() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::PendingDocs(vec![doc(1)]));
        let (req, _) = sends(&c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: BuildOutcome::NeedsServerCopy,
        }))
        .pop()
        .unwrap();
        let fx = c.step(Input::Reply {
            req,
            result: Err(ServerError {
                current_seq: Some(12),
                ..ServerError::new("deleted")
            }),
        });
        assert!(fx.contains(&Effect::ApplyServerDeleted {
            doc_id: doc(1),
            seq: 12
        }));
    }

    #[test]
    fn mismatch_attempts_accumulate_and_reset() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::Settled {
            doc_id: doc(1),
            outcome: SettleOutcome::Retry {
                after_ms: None,
                mismatch: true,
            },
        });
        let fx = c.step(Input::Settled {
            doc_id: doc(1),
            outcome: SettleOutcome::FetchServerCopy,
        });
        let (get_req, _) = sends(&fx).pop().unwrap();
        c.step(Input::Reply {
            req: get_req,
            result: Ok(Response::Document(envelope(doc(1), 3))),
        });
        c.step(Input::ServerCopyApplied { doc_id: doc(1) });
        c.step(Input::PendingDocs(vec![doc(1)]));
        let req = upload_req(&c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: prepared(doc(1)),
        }));
        let fx = c.step(Input::Reply {
            req,
            result: Err(ServerError::new("hash_mismatch")),
        });
        assert!(fx.iter().any(|e| matches!(
            e,
            Effect::SettleUpload {
                mismatch_attempts: 2,
                ..
            }
        )));
        c.step(Input::Settled {
            doc_id: doc(1),
            outcome: SettleOutcome::Done { rows_remain: false },
        });
        c.step(Input::PendingDocs(vec![doc(1)]));
        let req = upload_req(&c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: prepared(doc(1)),
        }));
        let fx = c.step(Input::Reply {
            req,
            result: Ok(Response::Uploaded(envelope(doc(1), 4))),
        });
        assert!(fx.iter().any(|e| matches!(
            e,
            Effect::SettleUpload {
                mismatch_attempts: 0,
                ..
            }
        )));
    }

    #[test]
    fn transient_retry_backs_off_per_document() {
        let mut c = core();
        connected(&mut c);
        let fx = c.step(Input::Settled {
            doc_id: doc(1),
            outcome: SettleOutcome::Retry {
                after_ms: None,
                mismatch: false,
            },
        });
        assert!(fx.contains(&Effect::Schedule {
            timer: TimerId::DocRetry(doc(1)),
            after: Duration::from_secs(1)
        }));
        assert!(!c
            .step(Input::PendingDocs(vec![doc(1)]))
            .iter()
            .any(|e| matches!(e, Effect::BuildUpload { .. })));
        let fx = c.step(Input::Timer(TimerId::DocRetry(doc(1))));
        assert!(fx.contains(&Effect::BuildUpload { doc_id: doc(1) }));
        let fx = c.step(Input::Settled {
            doc_id: doc(1),
            outcome: SettleOutcome::Retry {
                after_ms: None,
                mismatch: false,
            },
        });
        assert!(fx.contains(&Effect::Schedule {
            timer: TimerId::DocRetry(doc(1)),
            after: Duration::from_secs(2)
        }));
    }

    #[test]
    fn upload_timeout_releases_document_with_backoff() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::PendingDocs(vec![doc(1)]));
        let req = upload_req(&c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: prepared(doc(1)),
        }));
        let fx = c.step(Input::Timer(TimerId::Request(req)));
        assert!(fx.contains(&Effect::Schedule {
            timer: TimerId::DocRetry(doc(1)),
            after: Duration::from_secs(1)
        }));
    }

    fn builds(fx: &[Effect]) -> bool {
        fx.iter().any(|e| matches!(e, Effect::BuildUpload { .. }))
    }

    #[test]
    fn doc_is_not_rebuilt_between_upload_reply_and_settled() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::PendingDocs(vec![doc(1)]));
        let req = upload_req(&c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: prepared(doc(1)),
        }));
        c.step(Input::Reply {
            req,
            result: Ok(Response::Uploaded(envelope(doc(1), 9))),
        });
        assert!(!builds(&c.step(Input::PendingDocs(vec![doc(1)]))));
        c.step(Input::Settled {
            doc_id: doc(1),
            outcome: SettleOutcome::Done { rows_remain: false },
        });
        assert!(builds(&c.step(Input::PendingDocs(vec![doc(1)]))));
    }

    #[test]
    fn settling_docs_count_toward_in_flight_limit() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::PendingDocs((0..8).map(doc).collect()));
        for n in 0..8 {
            let req = upload_req(&c.step(Input::UploadBuilt {
                doc_id: doc(n),
                outcome: prepared(doc(n)),
            }));
            c.step(Input::Reply {
                req,
                result: Ok(Response::Uploaded(envelope(doc(n), 9))),
            });
        }
        assert!(!builds(&c.step(Input::PendingDocs(vec![doc(9)]))));
        let fx = c.step(Input::Settled {
            doc_id: doc(0),
            outcome: SettleOutcome::Done { rows_remain: false },
        });
        assert!(fx.contains(&Effect::LoadPending));
    }

    #[test]
    fn doc_is_not_rebuilt_between_document_reply_and_server_copy_applied() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::PendingDocs(vec![doc(1)]));
        let (req, _) = sends(&c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: BuildOutcome::NeedsServerCopy,
        }))
        .pop()
        .unwrap();
        c.step(Input::Reply {
            req,
            result: Ok(Response::Document(envelope(doc(1), 3))),
        });
        assert!(!builds(&c.step(Input::PendingDocs(vec![doc(1)]))));
        assert!(builds(&c.step(Input::ServerCopyApplied { doc_id: doc(1) })));
    }

    #[test]
    fn fatal_upload_reply_halts() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::PendingDocs(vec![doc(1)]));
        let req = upload_req(&c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: prepared(doc(1)),
        }));
        let fx = c.step(Input::Reply {
            req,
            result: Err(ServerError {
                is_fatal: true,
                ..ServerError::new("auth_invalid")
            }),
        });
        assert!(emitted(&fx).contains(&Lifecycle::ConnectionLost));
        assert!(!fx.iter().any(|e| matches!(e, Effect::SettleUpload { .. })));
        assert_eq!(
            c.state().connection,
            ConnectionView::Halted(HaltReason::AuthInvalid)
        );
    }

    #[test]
    fn fatal_get_document_reply_halts() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::PendingDocs(vec![doc(1)]));
        let (req, _) = sends(&c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: BuildOutcome::NeedsServerCopy,
        }))
        .pop()
        .unwrap();
        let fx = c.step(Input::Reply {
            req,
            result: Err(ServerError {
                is_fatal: true,
                ..ServerError::new("update_required")
            }),
        });
        assert!(emitted(&fx).contains(&Lifecycle::ConnectionLost));
        assert_eq!(
            c.state().connection,
            ConnectionView::Halted(HaltReason::UpdateRequired)
        );
    }

    #[test]
    fn get_document_honours_retry_after() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::PendingDocs(vec![doc(1)]));
        let (req, _) = sends(&c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: BuildOutcome::NeedsServerCopy,
        }))
        .pop()
        .unwrap();
        let fx = c.step(Input::Reply {
            req,
            result: Err(ServerError {
                retry_after_ms: Some(9000),
                ..ServerError::new("rate_limited")
            }),
        });
        assert!(fx.contains(&Effect::Schedule {
            timer: TimerId::DocRetry(doc(1)),
            after: Duration::from_secs(9)
        }));
    }

    #[test]
    fn local_settle_resets_mismatch_attempts() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::Settled {
            doc_id: doc(1),
            outcome: SettleOutcome::Retry {
                after_ms: None,
                mismatch: true,
            },
        });
        c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: BuildOutcome::SettledLocally { rows_remain: false },
        });
        c.step(Input::PendingDocs(vec![doc(1)]));
        let req = upload_req(&c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: prepared(doc(1)),
        }));
        let fx = c.step(Input::Reply {
            req,
            result: Err(ServerError::new("hash_mismatch")),
        });
        assert!(fx.iter().any(|e| matches!(
            e,
            Effect::SettleUpload {
                mismatch_attempts: 0,
                ..
            }
        )));
    }

    #[test]
    fn nothing_to_upload_resets_doc_failures() {
        let mut c = core();
        connected(&mut c);
        let retry = || Input::Settled {
            doc_id: doc(1),
            outcome: SettleOutcome::Retry {
                after_ms: None,
                mismatch: false,
            },
        };
        c.step(retry());
        c.step(Input::Timer(TimerId::DocRetry(doc(1))));
        c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: BuildOutcome::Nothing,
        });
        assert!(c.step(retry()).contains(&Effect::Schedule {
            timer: TimerId::DocRetry(doc(1)),
            after: Duration::from_secs(1)
        }));
    }

    #[test]
    fn disconnect_forgets_in_flight_and_reconnect_pumps() {
        let mut c = core();
        connected(&mut c);
        c.step(Input::PendingDocs(vec![doc(1)]));
        let req = upload_req(&c.step(Input::UploadBuilt {
            doc_id: doc(1),
            outcome: prepared(doc(1)),
        }));
        c.step(closed(&c));
        assert!(c
            .step(Input::Reply {
                req,
                result: Ok(Response::Uploaded(envelope(doc(1), 9)))
            })
            .iter()
            .all(|e| !matches!(e, Effect::SettleUpload { .. })));
        c.step(Input::Timer(TimerId::Reconnect));
        let (jreq, _) = sends(&c.step(opened(&c))).pop().unwrap();
        c.step(Input::Reply {
            req: jreq,
            result: Ok(Response::Joined),
        });
        let fx = c.step(Input::Cursors(vec![
            ("own".into(), 5),
            ("collection:curated".into(), 5),
        ]));
        assert!(fx.iter().any(|e| matches!(
            e,
            Effect::Schedule {
                timer: TimerId::Pump,
                ..
            }
        )));
    }
}
