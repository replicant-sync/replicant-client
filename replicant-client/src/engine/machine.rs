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

    fn handle(&mut self, input: Input, fx: &mut Vec<Effect>) {
        if matches!(self.conn, Conn::Stopped) {
            return;
        }
        match input {
            Input::Shutdown => {
                fx.push(Effect::CloseSocket);
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
            Input::SocketOpened => {
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
            Input::SocketClosed => match self.conn {
                Conn::Connecting { .. } => self.fail_connect(None, fx),
                Conn::Connected(_) => self.lose_connection(fx),
                _ => {}
            },
            Input::Reply { req, result } => self.on_reply(req, result, fx),
            Input::Timer(timer) => self.on_timer(timer, fx),
            Input::Cursors(list) => self.on_cursors(list, fx),
            other => self.on_session_input(other, fx),
        }
    }

    fn connect_now(&mut self, fx: &mut Vec<Effect>) {
        self.conn = Conn::Connecting { join_req: None };
        fx.push(Effect::Emit(Lifecycle::ConnectionAttempted));
        fx.push(Effect::OpenSocket);
        fx.push(Effect::Schedule {
            timer: TimerId::ConnectTimeout,
            after: CONNECT_TIMEOUT,
        });
    }

    fn fail_connect(&mut self, retry_after_ms: Option<u64>, fx: &mut Vec<Effect>) {
        fx.push(Effect::Cancel(TimerId::ConnectTimeout));
        let delay = match retry_after_ms {
            Some(ms) => Duration::from_millis(ms),
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
        fx.push(Effect::CloseSocket);
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
                            fx.push(Effect::CloseSocket);
                            self.halt(reason, fx);
                        }
                        None => {
                            fx.push(Effect::CloseSocket);
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
                fx.push(Effect::CloseSocket);
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

    // Filled in by Task 11.
    fn start_catch_up(&mut self, _scope: &str, _fx: &mut Vec<Effect>) {}
    fn on_session_reply(
        &mut self,
        _req: u64,
        _p: Pending,
        _r: Result<Response, ServerError>,
        _fx: &mut Vec<Effect>,
    ) {
    }
    fn on_session_timer(&mut self, _t: TimerId, _fx: &mut Vec<Effect>) {}
    fn on_session_input(&mut self, _i: Input, _fx: &mut Vec<Effect>) {}
    // Filled in by Task 12.
    fn request_pump(&mut self, _fx: &mut Vec<Effect>) {}
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

#[cfg(test)]
mod connection_tests {
    use super::harness::*;
    use super::*;

    fn open_and_join(c: &mut Core) -> Vec<Effect> {
        let fx = c.step(Input::SocketOpened);
        let (req, request) = sends(&fx).pop().expect("join sent");
        assert_eq!(request, Request::Join);
        c.step(Input::Reply {
            req,
            result: Ok(Response::Joined),
        })
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
        assert!(fx.contains(&Effect::OpenSocket));
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
        let fx = c.step(Input::SocketOpened);
        let (req, _) = sends(&fx).pop().unwrap();
        let fx = c.step(Input::Reply {
            req,
            result: Err(ServerError::new("internal")),
        });
        assert_eq!(c.state().connection, ConnectionView::Disconnected);
        assert!(fx.iter().any(|e| matches!(e, Effect::Schedule { timer: TimerId::Reconnect, after } if *after < Duration::from_secs(1))));
        let fx = c.step(Input::Timer(TimerId::Reconnect));
        assert!(fx.contains(&Effect::OpenSocket));
    }

    #[test]
    fn server_retry_after_overrides_backoff() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        let (req, _) = sends(&c.step(Input::SocketOpened)).pop().unwrap();
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
        let (req, _) = sends(&c.step(Input::SocketOpened)).pop().unwrap();
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
        assert!(fx.contains(&Effect::CloseSocket));
    }

    #[test]
    fn auth_invalid_retries_via_credential_check() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        let (req, _) = sends(&c.step(Input::SocketOpened)).pop().unwrap();
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
        assert!(fx.contains(&Effect::OpenSocket));
    }

    #[test]
    fn unknown_code_follows_is_fatal() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        let (req, _) = sends(&c.step(Input::SocketOpened)).pop().unwrap();
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
        assert!(fx.contains(&Effect::CloseSocket));
        assert_eq!(c.state().connection, ConnectionView::Disconnected);
    }

    #[test]
    fn socket_closed_while_connected_emits_connection_lost_once() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        open_and_join(&mut c);
        let fx = c.step(Input::SocketClosed);
        assert_eq!(emitted(&fx), vec![Lifecycle::ConnectionLost]);
        assert!(emitted(&c.step(Input::SocketClosed)).is_empty());
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
        c.step(Input::SocketClosed);
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
        assert!(!c.step(Input::Reconnect).contains(&Effect::OpenSocket));
        open_and_join(&mut c);
        assert!(!c.step(Input::Reconnect).contains(&Effect::OpenSocket));
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
        let fx = c.step(Input::SocketClosed);
        assert!(fx.iter().any(|e| matches!(e, Effect::Schedule { timer: TimerId::Reconnect, after } if *after < Duration::from_secs(1))));
    }

    #[test]
    fn shutdown_stops_everything() {
        let mut c = core();
        c.step(Input::Start {
            has_credentials: true,
        });
        open_and_join(&mut c);
        let fx = c.step(Input::Shutdown);
        assert!(fx.contains(&Effect::CloseSocket));
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
}
