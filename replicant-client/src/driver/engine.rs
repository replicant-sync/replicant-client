//! One owner task per engine. It steps the sans-IO core, runs every effect against the store,
//! the socket and the timers in order, and feeds the answers back through one FIFO queue.
//! Nothing here awaits the network: replies arrive as socket events.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch, Notify};
use tokio::task::JoinHandle;
use tokio::time::{Interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::timers::Timers;
use crate::engine::list_merge::ListMergeConfig;
use crate::engine::machine::{
    BuildOutcome, ConnectionView, Core, Effect, EngineState, HaltReason, Input, Lifecycle, Request,
    Response, SettleOutcome, TimerId,
};
use crate::engine::types::ServerError;
use crate::store::change_log::{ChangeLogReader, ChangeOrigin, DocChange, LogRead};
use crate::store::{now_unix, DocNotice, IdentityCheck, Store, StoreError, StoreResult};
use crate::transport::connection::{Connection, Received};
use crate::transport::socket::SocketEvent;
use crate::transport::wire::{socket_url, user_agent, JoinAuth};

#[cfg(test)]
mod e2e_tests;
#[cfg(test)]
mod event_tests;
#[cfg(test)]
mod owner_support;
#[cfg(test)]
mod storm_tests;
#[cfg(test)]
mod tests;

const COMMAND_BUFFER: usize = 32;
/// How often the change log is read when nothing else wakes the owner (spec §8).
const LOG_TICK: Duration = Duration::from_millis(250);
/// While connected, the stored credentials are read every this many ticks (once a second).
const SIGNED_IN_CHECK_TICKS: u32 = 4;

/// Reads the stored credentials: `Ok(None)` = signed out; `Err` = present but unreadable right
/// now (a torn or corrupt file), which never counts as a sign-out.
pub type CredentialLoader = Arc<dyn Fn() -> io::Result<Option<JoinAuth>> + Send + Sync>;

pub struct EngineConfig {
    /// http(s) or ws(s), with or without `/socket/websocket`.
    pub server_url: String,
    /// Names the host in the `User-Agent`, e.g. "Entonal Studio" and "2.0.1".
    pub host_app: String,
    pub host_version: String,
    pub credentials: CredentialLoader,
    pub jitter_seed: u64,
    /// How lists that both this device and another changed are merged. Local to this device.
    pub list_merge: ListMergeConfig,
}

/// Idempotent, so a full command queue loses nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Reconnect,
    /// The stored credentials changed; the engine reads them again.
    CredentialsChanged,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    Lifecycle(Lifecycle),
    /// Raised by a rule this engine applied (`ConflictDetected`, a document `SyncError`).
    Doc(DocNotice),
    /// One document changed: by this engine's host, another process's host, or by sync.
    Changed(DocChange),
    /// Changes were trimmed before this engine read them; the host reloads its lists.
    DatabaseChanged,
    /// The join's user id differs from the one this engine started with: this engine or another
    /// process adopted it and restamped the owned documents.
    IdentityAdopted {
        user_id: Uuid,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("invalid engine config: {0}")]
    Config(String),
}

pub struct Engine {
    controls: Controls,
    task: JoinHandle<()>,
}

/// The engine's ends of the owner's channels.
struct Controls {
    store: Arc<Store>,
    commands: mpsc::Sender<Command>,
    outbox: Arc<Notify>,
    state: watch::Receiver<EngineState>,
    cancel: CancellationToken,
}

impl Engine {
    /// Opens the store at `db_path` (creating its `user_config` row if needed) and starts the
    /// owner. Returns without waiting for the network.
    pub async fn start(
        db_path: &Path,
        config: EngineConfig,
        events: mpsc::UnboundedSender<EngineEvent>,
    ) -> Result<Engine, EngineError> {
        let (owner, controls) = Owner::open(db_path, config, events).await?;
        let task = tokio::spawn(owner.run());
        Ok(Engine { controls, task })
    }

    /// For the host's reads and writes; call `notify_outbox` after every write.
    pub fn store(&self) -> Arc<Store> {
        self.controls.store.clone()
    }

    pub fn notify_outbox(&self) {
        self.controls.outbox.notify_one();
    }

    /// False when the command queue is full; commands are idempotent, so nothing is lost.
    pub fn command(&self, command: Command) -> bool {
        self.controls.commands.try_send(command).is_ok()
    }

    pub fn state(&self) -> EngineState {
        self.controls.state.borrow().clone()
    }

    /// Stops the owner without waiting on the network, then closes the store.
    pub async fn stop(self) {
        self.stop_owner().await.close().await;
    }

    /// Stops the owner without waiting on the network; the caller closes the returned store.
    pub async fn stop_owner(mut self) -> Arc<Store> {
        self.controls.cancel.cancel();
        if let Err(error) = (&mut self.task).await {
            warn!(%error, "engine owner task failed");
        }
        self.controls.store.clone()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.controls.cancel.cancel();
    }
}

/// An input waiting to be stepped. `epoch` is the socket generation current when the effect
/// that produced a DB answer ran; `None` for inputs that belong to no session.
struct Queued {
    epoch: Option<u64>,
    input: Input,
}

enum Wake {
    Cancelled,
    Command(Command),
    Socket(SocketEvent),
    Timer(TimerId),
    Outbox,
    Tick,
}

struct Owner {
    core: Core,
    store: Arc<Store>,
    /// The user this data dir belongs to; set again by every join's identity check.
    me: Uuid,
    connection: Connection,
    timers: Timers,
    reader: ChangeLogReader,
    credentials: CredentialLoader,
    /// Fingerprint of the credentials `connection` signs joins with.
    auth_fingerprint: Option<[u8; 32]>,
    /// Fingerprint of the credentials that signed the most recently sent join; may differ from
    /// `auth_fingerprint` if credentials changed while that join was outstanding.
    join_fingerprint: Option<[u8; 32]>,
    /// Fingerprint of credentials the server refused (`auth_invalid`, `account_disabled`), or that joined as
    /// another user (`identity_drift`). Cleared by a sign-out: the same keys stored again are dialled.
    rejected: Option<[u8; 32]>,
    /// Seconds added to this machine's clock when signing a join; learnt from `clock_skew`.
    clock_offset: i64,
    socket_gen: u64,
    queue: VecDeque<Queued>,
    events: mpsc::UnboundedSender<EngineEvent>,
    state: watch::Sender<EngineState>,
    commands: mpsc::Receiver<Command>,
    outbox: Arc<Notify>,
    cancel: CancellationToken,
    tick: Interval,
    /// Ticks since the stored credentials were last read while connected.
    ticks_since_signed_in_check: u32,
    #[cfg(test)]
    dropped_stale: usize,
}

impl Owner {
    async fn open(
        db_path: &Path,
        config: EngineConfig,
        events: mpsc::UnboundedSender<EngineEvent>,
    ) -> Result<(Owner, Controls), EngineError> {
        config.list_merge.validate().map_err(EngineError::Config)?;
        socket_url(&config.server_url, Uuid::nil()).map_err(EngineError::Config)?;
        let mut store = retry_open(|| Store::open(db_path)).await?;
        store.list_merge = config.list_merge.clone();
        let store = Arc::new(store);
        match Self::prepare(store.clone(), config, events).await {
            Ok(opened) => Ok(opened),
            Err(error) => {
                store.close().await;
                Err(error)
            }
        }
    }

    async fn prepare(
        store: Arc<Store>,
        config: EngineConfig,
        events: mpsc::UnboundedSender<EngineEvent>,
    ) -> Result<(Owner, Controls), EngineError> {
        let client_id = store.ensure_user_config(&config.server_url).await?;
        let url = socket_url(&config.server_url, client_id).map_err(EngineError::Config)?;
        let me = store.user_id().await?;
        let scopes = store.subscribed_scopes().await?;
        let reader = ChangeLogReader::open(&store, now_unix()).await?;
        let connection = Connection::new(
            url,
            None,
            user_agent(&config.host_app, &config.host_version),
        );
        let core = Core::new(scopes, config.jitter_seed);
        let (state_tx, state_rx) = watch::channel(core.state());
        let (command_tx, command_rx) = mpsc::channel(COMMAND_BUFFER);
        let outbox = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let mut tick = tokio::time::interval(LOG_TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let owner = Owner {
            core,
            store: store.clone(),
            me,
            connection,
            timers: Timers::default(),
            reader,
            credentials: config.credentials,
            auth_fingerprint: None,
            join_fingerprint: None,
            rejected: None,
            clock_offset: 0,
            socket_gen: 0,
            queue: VecDeque::new(),
            events,
            state: state_tx,
            commands: command_rx,
            outbox: outbox.clone(),
            cancel: cancel.clone(),
            tick,
            ticks_since_signed_in_check: 0,
            #[cfg(test)]
            dropped_stale: 0,
        };
        let controls = Controls {
            store,
            commands: command_tx,
            outbox,
            state: state_rx,
            cancel,
        };
        Ok((owner, controls))
    }

    async fn run(mut self) {
        self.start().await;
        while self.turn().await {}
        self.shutdown().await;
    }

    /// An unreadable credential file at start is not a sign-out: the join-time read decides.
    async fn start(&mut self) {
        let has_credentials = self.load_credentials().unwrap_or_else(|error| {
            warn!(%error, "stored credentials unreadable at start");
            true
        });
        self.feed(Input::Start { has_credentials }).await;
    }

    /// Steps `Shutdown` (the socket close is detached) and removes this engine's change-log
    /// reader so it never holds back trimming.
    async fn shutdown(mut self) {
        self.feed(Input::Shutdown).await;
        if let Err(error) = self.reader.close().await {
            warn!(%error, "closing the change-log reader failed");
        }
    }

    /// Waits for one wake-up and handles it; false once cancelled.
    async fn turn(&mut self) -> bool {
        let wake = self.wait().await;
        if matches!(wake, Wake::Cancelled) {
            return false;
        }
        self.handle(wake).await;
        true
    }

    /// Socket events come before timers, so a reply that has already arrived is handled before
    /// a timeout due at the same instant.
    async fn wait(&mut self) -> Wake {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Wake::Cancelled,
            Some(command) = self.commands.recv() => Wake::Command(command),
            event = self.connection.next_event() => Wake::Socket(event),
            timer = self.timers.next() => Wake::Timer(timer),
            _ = self.outbox.notified() => Wake::Outbox,
            _ = self.tick.tick() => Wake::Tick,
        }
    }

    async fn handle(&mut self, wake: Wake) {
        match wake {
            Wake::Cancelled => {}
            Wake::Command(Command::Reconnect) => self.feed(Input::Reconnect).await,
            Wake::Command(Command::CredentialsChanged) => match self.load_credentials() {
                Ok(has_credentials) => {
                    self.feed(Input::CredentialsChanged { has_credentials })
                        .await
                }
                Err(error) => warn!(%error, "stored credentials unreadable; keeping the last ones"),
            },
            Wake::Socket(event) => {
                if let Some(received) = self.connection.receive(event) {
                    let input = self.on_received(received).await;
                    self.feed(input).await;
                }
            }
            Wake::Timer(timer) => self.feed(Input::Timer(timer)).await,
            Wake::Outbox => {
                self.read_change_log().await;
                self.feed(Input::OutboxChanged).await;
            }
            Wake::Tick => {
                if self.read_change_log().await {
                    self.feed(Input::OutboxChanged).await;
                }
                self.check_still_signed_in().await;
            }
        }
    }

    /// A sign-out in another process is never announced here: while connected, the stored
    /// credentials are read once a second and their removal ends the connection. New or
    /// unreadable credentials change nothing (the next join reads them again).
    async fn check_still_signed_in(&mut self) {
        if self.core.state().connection != ConnectionView::Connected {
            self.ticks_since_signed_in_check = 0;
            return;
        }
        self.ticks_since_signed_in_check += 1;
        if self.ticks_since_signed_in_check < SIGNED_IN_CHECK_TICKS {
            return;
        }
        self.ticks_since_signed_in_check = 0;
        if let Ok(None) = (self.credentials)() {
            self.feed(Input::CredentialsChanged {
                has_credentials: false,
            })
            .await;
        }
    }

    /// A `clock_skew` join reply carries the server's clock; every later join is signed with it.
    fn learn_clock_offset(&mut self, input: &Input) {
        if let Input::Reply {
            result:
                Err(ServerError {
                    code,
                    server_time: Some(server_time),
                    ..
                }),
            ..
        } = input
        {
            if code == "clock_skew" {
                self.clock_offset = server_time - now_unix();
            }
        }
    }

    /// A join reaches the core only after its user id was checked against the data dir:
    /// adopted on the first join, and any other id afterwards halts as `identity_drift`.
    async fn on_received(&mut self, received: Received) -> Input {
        let (req, user_id) = match received {
            Received::Input(input) => {
                self.learn_clock_offset(&input);
                return input;
            }
            Received::Joined { req, user_id } => (req, user_id),
        };
        let result = match self.store.check_identity(user_id).await {
            Ok(IdentityCheck::Matches) => Ok(Response::Joined),
            Ok(IdentityCheck::Adopted { previous }) => {
                info!(%previous, %user_id, "adopted the server's user id");
                Ok(Response::Joined)
            }
            Ok(IdentityCheck::Drift { local }) => {
                warn!(%local, %user_id, "server user id differs from the adopted one");
                let mut drift = ServerError::new("identity_drift");
                drift.is_fatal = true;
                Err(drift)
            }
            Err(error) => {
                warn!(%error, "identity check failed");
                Err(ServerError::new("store_error"))
            }
        };
        if result.is_ok() {
            if self.me != user_id {
                self.emit(EngineEvent::IdentityAdopted { user_id });
            }
            self.me = user_id;
            self.rejected = None;
        }
        Input::Reply { req, result }
    }

    /// Reads the stored credentials: what is stored now signs the next join, and with nothing
    /// stored nothing may join. Runs on every explicit change and before every join, so a
    /// sign-out in another process is honoured at this engine's next dial. An unreadable file
    /// changes nothing: the last credentials loaded stay.
    fn load_credentials(&mut self) -> io::Result<bool> {
        match (self.credentials)()? {
            Some(auth) => {
                self.auth_fingerprint = Some(fingerprint(&auth));
                self.connection.set_auth(auth);
                Ok(true)
            }
            None => {
                self.auth_fingerprint = None;
                self.rejected = None;
                Ok(false)
            }
        }
    }

    /// A halted engine's periodic check: credentials the server rejected with `auth_invalid` or
    /// `account_disabled`, or that joined as another user, are not tried again until they change.
    fn check_credentials(&mut self) -> Input {
        let auth = match (self.credentials)() {
            Ok(Some(auth)) => auth,
            Ok(None) => {
                self.auth_fingerprint = None;
                self.rejected = None;
                return Input::CredentialsChanged {
                    has_credentials: false,
                };
            }
            Err(error) => {
                warn!(%error, "stored credentials unreadable");
                return Input::CredentialsUnchanged;
            }
        };
        let print = fingerprint(&auth);
        if self.rejected == Some(print) {
            return Input::CredentialsUnchanged;
        }
        self.auth_fingerprint = Some(print);
        self.connection.set_auth(auth);
        Input::CredentialsChanged {
            has_credentials: true,
        }
    }

    async fn feed(&mut self, input: Input) {
        self.queue.push_back(Queued { epoch: None, input });
        self.drain().await;
    }

    /// Steps queued inputs until none are left. A DB answer from an older socket generation is
    /// dropped: a previous session's answer must not reach the new session.
    async fn drain(&mut self) {
        while let Some(Queued { epoch, input }) = self.queue.pop_front() {
            if epoch.is_some_and(|epoch| epoch != self.socket_gen) {
                debug!(?input, "dropping an answer from a previous socket");
                #[cfg(test)]
                {
                    self.dropped_stale += 1;
                }
                continue;
            }
            self.apply_step(input).await;
        }
    }

    /// Steps one input and runs its effects in order. A send failure is queued only after the
    /// step's remaining effects have run.
    async fn apply_step(&mut self, input: Input) {
        let effects = self.core.step(input);
        let mut send_failed = None;
        for effect in effects {
            if let Some(closed) = self.execute(effect).await {
                send_failed.get_or_insert(closed);
            }
        }
        if let Some(closed) = send_failed {
            self.queue.push_back(Queued {
                epoch: None,
                input: closed,
            });
        }
    }

    /// Runs one effect; DB answers go to the back of the queue. Returns the `SocketClosed` a
    /// failed send produced.
    async fn execute(&mut self, effect: Effect) -> Option<Input> {
        let store = self.store.clone();
        let me = self.me;
        match effect {
            Effect::OpenSocket { gen } => {
                self.socket_gen = gen;
                self.connection.open(gen);
            }
            Effect::CloseSocket { gen } => self.connection.close(gen),
            Effect::Send { req, request } => {
                if matches!(request, Request::Join) {
                    match self.load_credentials() {
                        Ok(true) => self.join_fingerprint = self.auth_fingerprint,
                        Ok(false) => {
                            // Signed out since this dial began: the core halts instead of joining.
                            self.queue.push_back(Queued {
                                epoch: None,
                                input: Input::CredentialsChanged {
                                    has_credentials: false,
                                },
                            });
                            return None;
                        }
                        Err(error) => {
                            // Not a sign-out: drop this socket so the core backs off and dials again.
                            warn!(%error, "stored credentials unreadable; retrying the dial");
                            let gen = self.socket_gen;
                            self.connection.close(gen);
                            self.queue.push_back(Queued {
                                epoch: Some(gen),
                                input: Input::SocketClosed { gen },
                            });
                            return None;
                        }
                    }
                }
                // Marked before it can reach the server: an upload must never be sent unmarked.
                if let Request::Upload(upload) = &request {
                    if let Err(error) = store.mark_sent(upload.doc_id, upload.upload_id).await {
                        warn!(%error, doc_id = %upload.doc_id, "could not mark an upload as sent; not sending it");
                        // Times the request out now, so the document backs off at once.
                        self.answer(Input::Timer(TimerId::Request(req)));
                        return None;
                    }
                }
                return self
                    .connection
                    .send(req, &request, now_unix() + self.clock_offset);
            }
            Effect::Schedule { timer, after } => self.timers.schedule(timer, after),
            Effect::Cancel(timer) => self.timers.cancel(&timer),
            Effect::CheckCredentials => {
                let input = self.check_credentials();
                self.queue.push_back(Queued { epoch: None, input });
            }
            Effect::LoadCursors => match twice(|| store.load_cursors()).await {
                Ok(cursors) => self.answer(Input::Cursors(cursors)),
                Err(error) => self.force_reconnect("load_cursors", &error),
            },
            Effect::ApplyChanges {
                scope,
                changes,
                new_cursor,
                tag,
            } => match twice(|| store.apply_changes(me, &scope, &changes, new_cursor)).await {
                Ok(notices) => {
                    self.notify_host(notices);
                    self.answer(Input::Applied { scope, tag });
                }
                Err(error) => self.force_reconnect("apply_changes", &error),
            },
            Effect::ApplySnapshotPage { scope, docs, tag } => {
                match twice(|| store.apply_snapshot_page(me, &scope, &docs)).await {
                    Ok(notices) => {
                        self.notify_host(notices);
                        self.answer(Input::Applied { scope, tag });
                    }
                    Err(error) => self.force_reconnect("apply_snapshot_page", &error),
                }
            }
            Effect::FinishSnapshot {
                scope,
                seen,
                snapshot_seq,
                tag,
            } => match twice(|| store.finish_snapshot(&scope, &seen, snapshot_seq)).await {
                Ok(()) => self.answer(Input::Applied { scope, tag }),
                Err(error) => self.force_reconnect("finish_snapshot", &error),
            },
            Effect::LoadPending => match store.load_pending().await {
                Ok(docs) => self.answer(Input::PendingDocs(docs)),
                Err(error) => {
                    warn!(%error, "load_pending failed; retrying");
                    self.answer(Input::PendingLoadFailed);
                }
            },
            Effect::BuildUpload { doc_id } => {
                let outcome = store
                    .build_upload(me, doc_id)
                    .await
                    .unwrap_or_else(|error| {
                        warn!(%error, %doc_id, "build_upload failed; the document backs off");
                        BuildOutcome::Failed
                    });
                self.answer(Input::UploadBuilt { doc_id, outcome });
            }
            Effect::SettleUpload {
                doc_id,
                inflight,
                reply,
                mismatch_attempts,
                divergent_replies,
            } => {
                let outcome = match store
                    .settle_upload(
                        me,
                        doc_id,
                        &inflight,
                        &reply,
                        mismatch_attempts,
                        divergent_replies,
                    )
                    .await
                {
                    Ok((outcome, notices)) => {
                        self.notify_host(notices);
                        outcome
                    }
                    Err(error) => {
                        // The rows stay; the resend has the same upload id and gets the stored reply.
                        warn!(%error, %doc_id, "settle_upload failed");
                        SettleOutcome::Retry {
                            after_ms: None,
                            mismatch: false,
                        }
                    }
                };
                self.answer(Input::Settled { doc_id, outcome });
            }
            Effect::ApplyServerCopy { doc_id, doc } => {
                match store.apply_server_copy(me, doc_id, doc.as_ref()).await {
                    Ok(notices) => self.notify_host(notices),
                    Err(error) => warn!(%error, %doc_id, "apply_server_copy failed"),
                }
                self.answer(Input::ServerCopyApplied { doc_id });
            }
            Effect::ApplyServerDeleted { doc_id, seq } => {
                match store.apply_server_deleted(doc_id, seq).await {
                    Ok(notices) => self.notify_host(notices),
                    Err(error) => warn!(%error, %doc_id, "apply_server_deleted failed"),
                }
                self.answer(Input::ServerCopyApplied { doc_id });
            }
            Effect::DropSubscription { scope } => {
                if let Err(error) = store.drop_subscription(&scope).await {
                    warn!(%error, %scope, "drop_subscription failed");
                }
            }
            Effect::Emit(lifecycle) => self.emit(EngineEvent::Lifecycle(lifecycle)),
            Effect::SetState(state) => {
                let refused = match &state.connection {
                    ConnectionView::Halted(HaltReason::Other(code)) => code == "identity_drift",
                    ConnectionView::Halted(reason) => matches!(
                        reason,
                        HaltReason::AuthInvalid | HaltReason::AccountDisabled
                    ),
                    _ => false,
                };
                if refused {
                    self.rejected = self.join_fingerprint;
                }
                self.state.send_replace(state);
            }
        }
        None
    }

    fn answer(&mut self, input: Input) {
        self.queue.push_back(Queued {
            epoch: Some(self.socket_gen),
            input,
        });
    }

    /// A feed effect failed twice. The core has no apply timeout and must hear something:
    /// closing the socket makes it reconnect and catch up again from the stored cursor.
    fn force_reconnect(&mut self, what: &str, error: &StoreError) {
        warn!(%error, what, "store failed twice; reconnecting");
        let gen = self.socket_gen;
        self.connection.close(gen);
        self.queue.push_back(Queued {
            epoch: Some(gen),
            input: Input::SocketClosed { gen },
        });
    }

    fn notify_host(&self, notices: Vec<DocNotice>) {
        for notice in notices {
            self.emit(EngineEvent::Doc(notice));
        }
    }

    fn emit(&self, event: EngineEvent) {
        let _ = self.events.send(event);
    }

    /// Emits one event per document changed since the last read. True when another process's
    /// host wrote (its outbox rows may be uploaded from here too) or rows were trimmed.
    async fn read_change_log(&mut self) -> bool {
        match self.reader.read(now_unix()).await {
            Ok(LogRead::Docs(changes)) => {
                let others_wrote = changes
                    .iter()
                    .any(|change| change.origin == ChangeOrigin::OtherProcess);
                for change in changes {
                    self.emit(EngineEvent::Changed(change));
                }
                others_wrote
            }
            Ok(LogRead::DatabaseChanged) => {
                self.emit(EngineEvent::DatabaseChanged);
                true
            }
            Err(error) => {
                warn!(%error, "change-log read failed");
                false
            }
        }
    }
}

/// Identifies credentials without keeping the secret.
fn fingerprint(auth: &JoinAuth) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(auth.email.as_bytes());
    hasher.update([0]);
    hasher.update(auth.api_key.as_bytes());
    hasher.update([0]);
    hasher.update(auth.api_secret.as_bytes());
    hasher.finalize().into()
}

/// Feed effects are retried once before the owner gives up on the connection.
async fn twice<T, Fut>(mut attempt: impl FnMut() -> Fut) -> StoreResult<T>
where
    Fut: Future<Output = StoreResult<T>>,
{
    match attempt().await {
        Err(error) => {
            warn!(%error, "store effect failed; retrying once");
            attempt().await
        }
        done => done,
    }
}

const OPEN_ATTEMPTS: u64 = 4;

/// Two processes opening a pre-v2 data dir at once race the migrations (sqlx's SQLite migrator
/// takes no lock) and the connect; the loser retries until the winner has finished.
async fn retry_open<T, Fut>(mut open: impl FnMut() -> Fut) -> StoreResult<T>
where
    Fut: Future<Output = StoreResult<T>>,
{
    let mut attempt = 1;
    loop {
        match open().await {
            Err(error) if attempt < OPEN_ATTEMPTS && lost_an_open_race(&error) => {
                warn!(%error, attempt, "store open raced another process; retrying");
                tokio::time::sleep(Duration::from_millis(50 * attempt)).await;
                attempt += 1;
            }
            done => return done,
        }
    }
}

fn lost_an_open_race(error: &StoreError) -> bool {
    match error {
        StoreError::Migrate(_) => !error.is_newer_schema(),
        _ => error.is_busy(),
    }
}
