//! One engine per database per process: every handle on a data dir shares its socket, runtime
//! and sync; the last handle to close stops it.

mod fanout;
#[cfg(test)]
mod tests;

pub use fanout::{HostEvent, Origin};

use std::collections::HashMap;
use std::ffi::OsStr;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};
use std::thread::JoinHandle as ThreadHandle;
use std::time::{Duration, Instant};

use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::driver::engine::{Command, CredentialLoader, Engine, EngineConfig, EngineError};
use crate::engine::list_merge::ListMergeConfig;
use crate::engine::machine::EngineState;
use crate::secret_store;
use crate::store::{is_json_pointer, Store};
use crate::transport::wire::JoinAuth;
use fanout::{EventQueue, FanOut};

/// How long a stopping engine's runtime may take to end its threads.
const RUNTIME_JOIN: Duration = Duration::from_secs(2);
const WORKER_THREADS: usize = 2;

/// One per database key. Its lock is held while that key's engine starts and while the slot is
/// emptied, not while the engine stops (that runs on the teardown thread), so the registry lock
/// itself is only ever held for a map lookup.
#[derive(Default)]
struct Slot(Mutex<Weak<Shared>>);

static REGISTRY: LazyLock<Mutex<HashMap<PathBuf, Arc<Slot>>>> = LazyLock::new(Default::default);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    /// Holds the database file and the stored credentials.
    pub data_dir: PathBuf,
    pub database_file: String,
    pub server_url: String,
    /// Signs joins when the stored credentials carry no email (stored by 0.6).
    pub fallback_email: Option<String>,
    /// Names the host in the `User-Agent`, e.g. "Entonal Studio" and "2.0.1 CLAP".
    pub host_app: String,
    pub host_version: String,
    /// How lists both sides changed are merged; one policy per engine.
    pub list_merge: ListMergeConfig,
    /// JSON Pointer to each document's title; `None`: no titles. One per engine.
    pub title_pointer: Option<String>,
}

impl HostConfig {
    /// Whether a handle with `other` may share this engine; the fallback email is not
    /// compared, since stored credentials normally carry their own.
    fn same_engine(&self, other: &HostConfig) -> bool {
        self.server_url == other.server_url
            && self.host_app == other.host_app
            && self.host_version == other.host_version
            && self.list_merge == other.list_merge
            && self.title_pointer == other.title_pointer
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("the database was migrated by a newer replicant-client")]
    NewerSchema,
    #[error("migrating the v1 data failed: {0}")]
    MigrationFailed(String),
    #[error("the database is locked by another process")]
    Busy,
    #[error("an engine is already open on {0} with a different configuration")]
    ConfigMismatch(PathBuf),
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error(transparent)]
    Engine(EngineError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

struct Shared {
    config: HostConfig,
    engine: Engine,
    fanout: Arc<Mutex<FanOut>>,
    fanout_task: JoinHandle<()>,
    runtime: Runtime,
}

impl Shared {
    /// Also returns the first handle's queue, attached before the fan-out runs so that no
    /// early event (an immediate not-enrolled halt) goes to zero queues.
    fn start(
        key: PathBuf,
        data_dir: PathBuf,
        config: HostConfig,
    ) -> Result<(Shared, Arc<EventQueue>), OpenError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(WORKER_THREADS)
            .thread_name("replicant-engine")
            .enable_all()
            .build()?;
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let engine_config = EngineConfig {
            server_url: config.server_url.clone(),
            host_app: config.host_app.clone(),
            host_version: config.host_version.clone(),
            credentials: credential_loader(data_dir, config.fallback_email.clone()),
            jitter_seed: rand::random(),
            list_merge: config.list_merge.clone(),
            title_pointer: config.title_pointer.clone(),
        };
        let engine = runtime
            .block_on(Engine::start(&key, engine_config, events_tx))
            .map_err(|error| match error {
                EngineError::Store(store) if store.is_newer_schema() => OpenError::NewerSchema,
                EngineError::Store(store) if store.is_migration_failed() => {
                    OpenError::MigrationFailed(store.to_string())
                }
                EngineError::Store(store) if store.is_busy() => OpenError::Busy,
                EngineError::Config(message) => OpenError::Config(message),
                other => OpenError::Engine(other),
            })?;
        let mut fanout = FanOut::default();
        let first = fanout.attach();
        let fanout = Arc::new(Mutex::new(fanout));
        let fanout_task = runtime.spawn(fanout::run(engine.store(), events_rx, fanout.clone()));
        #[cfg(test)]
        tests::after_fanout_spawn(&fanout);
        let shared = Shared {
            config,
            engine,
            fanout,
            fanout_task,
            runtime,
        };
        Ok((shared, first))
    }

    /// Never waits on the network; bounded by SQLite and `RUNTIME_JOIN`.
    fn stop(self) {
        let Shared {
            engine,
            fanout_task,
            runtime,
            ..
        } = self;
        runtime.block_on(async {
            let store = engine.stop_owner().await;
            if let Err(error) = fanout_task.await {
                tracing::warn!(%error, "the event fan-out ended abnormally");
            }
            store.close().await;
        });
        runtime.shutdown_timeout(RUNTIME_JOIN);
    }
}

/// Attaches to the engine for `config`'s database, starting it if this process has none.
/// Opens (and on first use migrates) the store; never waits on the network. Must not be called
/// from inside a tokio runtime.
pub fn attach(config: HostConfig) -> Result<Handle, OpenError> {
    config.list_merge.validate().map_err(OpenError::Config)?;
    if let Some(pointer) = config.title_pointer.as_deref() {
        if !is_json_pointer(pointer) {
            return Err(OpenError::Config(format!(
                "title pointer is not a JSON Pointer: {pointer}"
            )));
        }
    }
    if Path::new(&config.database_file).file_name() != Some(OsStr::new(&config.database_file)) {
        return Err(OpenError::Config(format!(
            "database_file must be a file name: {}",
            config.database_file
        )));
    }
    std::fs::create_dir_all(&config.data_dir)?;
    let data_dir = std::fs::canonicalize(&config.data_dir)?;
    let key = data_dir.join(&config.database_file);
    let slot = lock(&REGISTRY).entry(key.clone()).or_default().clone();
    let mut current = lock(&slot.0);
    if let Some(shared) = current.upgrade() {
        if !shared.config.same_engine(&config) {
            return Err(OpenError::ConfigMismatch(key));
        }
        drop(current);
        let queue = lock(&shared.fanout).attach();
        return Ok(Handle {
            shared: Some(shared),
            slot,
            queue,
        });
    }
    let (shared, queue) = Shared::start(key, data_dir, config)?;
    let shared = Arc::new(shared);
    *current = Arc::downgrade(&shared);
    drop(current);
    Ok(Handle {
        shared: Some(shared),
        slot,
        queue,
    })
}

/// Tells every engine in this process on `data_dir` that its stored credentials changed.
/// Returns how many were told. Waits for an engine of that data dir that is starting, never for
/// one on another data dir.
pub fn credentials_changed(data_dir: &Path) -> usize {
    let Ok(data_dir) = std::fs::canonicalize(data_dir) else {
        return 0;
    };
    let slots: Vec<Arc<Slot>> = lock(&REGISTRY)
        .iter()
        .filter(|(key, _)| key.parent() == Some(data_dir.as_path()))
        .map(|(_, slot)| slot.clone())
        .collect();
    slots
        .iter()
        .filter(|slot| {
            // The strong reference drops under the slot lock, so it can never be the last one.
            let current = lock(&slot.0);
            current
                .upgrade()
                .is_some_and(|shared| shared.engine.command(Command::CredentialsChanged))
        })
        .count()
}

/// One attachment to an engine. Not `Clone`: the slot counts handles through the `Arc`.
pub struct Handle {
    shared: Option<Arc<Shared>>,
    slot: Arc<Slot>,
    queue: Arc<EventQueue>,
}

impl Handle {
    fn shared(&self) -> &Shared {
        self.shared
            .as_deref()
            .expect("a handle is open until close")
    }

    pub fn store(&self) -> Arc<Store> {
        self.shared().engine.store()
    }

    /// Runs `future` on the engine's runtime. Must not be called from inside a tokio runtime.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.shared().runtime.block_on(future)
    }

    /// Call after every write, so the engine uploads it and reports it.
    pub fn notify_outbox(&self) {
        self.shared().engine.notify_outbox();
    }

    /// Leaves `Halted` or retries now (at most one dial per second).
    pub fn reconnect(&self) {
        self.shared().engine.command(Command::Reconnect);
    }

    pub fn state(&self) -> EngineState {
        self.shared().engine.state()
    }

    /// Everything queued for this handle since the last call, oldest first.
    pub fn take_events(&self) -> Vec<HostEvent> {
        self.queue.take()
    }

    /// Returns at once; the engine stops in the background if this was its last handle.
    pub fn close(mut self) -> Teardown {
        self.detach()
    }

    fn detach(&mut self) -> Teardown {
        let Some(shared) = self.shared.take() else {
            return Teardown(None);
        };
        lock(&shared.fanout).detach(&self.queue);
        let mut current = lock(&self.slot.0);
        if Arc::strong_count(&shared) > 1 {
            // Dropped under the lock, so two last handles closing together cannot both miss.
            drop(shared);
            drop(current);
            return Teardown(None);
        }
        *current = Weak::new();
        drop(current);
        let Ok(shared) = Arc::try_unwrap(shared) else {
            unreachable!("slots hold only weak references, and this was the last handle");
        };
        let worker = std::thread::Builder::new()
            .name("replicant-teardown".into())
            .spawn(move || shared.stop())
            .inspect_err(|error| tracing::warn!(%error, "could not spawn the teardown thread"))
            .ok();
        Teardown(worker)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _detached = self.detach();
    }
}

/// An engine stopping after its last handle closed.
pub struct Teardown(Option<ThreadHandle<()>>);

impl Teardown {
    /// Waits up to `timeout`; true when the engine has stopped, or when other handles keep it
    /// running and there was nothing to stop. Blocking-pool threads (a DNS lookup) can outlive
    /// the runtime's shutdown: a host that unloads the library after this must expect them to
    /// finish on their own.
    pub fn wait(self, timeout: Duration) -> bool {
        let Some(worker) = self.0 else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        while !worker.is_finished() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        worker.join().is_ok()
    }
}

/// `Err` when the stored files cannot be read right now; never reported as signed out. Damaged
/// files (see [`secret_store::is_damaged`]) read as signed out, so the user is asked to sign in.
fn credential_loader(data_dir: PathBuf, fallback_email: Option<String>) -> CredentialLoader {
    let damaged_reported = AtomicBool::new(false);
    Arc::new(move || {
        let stored = match secret_store::load(&data_dir) {
            Ok(stored) => stored,
            Err(error) if secret_store::is_damaged(&error) => {
                // Reported once per run of damaged reads, not at every recheck.
                if !damaged_reported.swap(true, Ordering::Relaxed) {
                    tracing::warn!(%error, "stored credentials are damaged; treating them as signed out");
                }
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        damaged_reported.store(false, Ordering::Relaxed);
        let Some(stored) = stored else {
            return Ok(None);
        };
        Ok(stored
            .email
            .or_else(|| fallback_email.clone())
            .map(|email| JoinAuth {
                email,
                api_key: stored.api_key,
                api_secret: stored.secret,
            }))
    })
}

/// A panic elsewhere must not stop every later caller.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
