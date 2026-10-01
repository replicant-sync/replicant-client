//! Engine test fixtures: a seeded data dir, credential loaders, and helpers for tests that mix
//! real sockets with long timers.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::engine::{CredentialLoader, EngineConfig, EngineEvent};
use crate::engine::list_merge::ListMergeConfig;
use crate::store::test_support::seed_user;
use crate::store::Store;
use crate::transport::wire::JoinAuth;

pub(crate) const WAIT: Duration = Duration::from_secs(5);
const DB_FILE: &str = "replicant.sqlite3";

/// A data dir whose `user_config` names `user_id`.
pub(crate) async fn seeded_db(user_id: Uuid, adopted: bool) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    let store = Store::open(&path).await.unwrap();
    seed_user(&store, user_id, adopted).await;
    store.close().await;
    (dir, path)
}

fn auth(api_key: &str) -> JoinAuth {
    auth_with_secret(api_key, "rps_test")
}

fn auth_with_secret(api_key: &str, api_secret: &str) -> JoinAuth {
    JoinAuth {
        email: "a@b.c".into(),
        api_key: api_key.into(),
        api_secret: api_secret.into(),
    }
}

pub(crate) fn credentials(api_key: &str) -> CredentialLoader {
    let api_key = api_key.to_string();
    Arc::new(move || Ok(Some(auth(&api_key))))
}

/// No stored credentials: the engine halts as not enrolled and never touches the network.
pub(crate) fn no_credentials() -> CredentialLoader {
    Arc::new(|| Ok(None))
}

/// Credentials a test can change, remove or make unreadable while the engine runs.
#[derive(Clone)]
pub(crate) struct SwitchableCredentials {
    stored: Arc<Mutex<Option<(String, String)>>>,
    unreadable: Arc<AtomicBool>,
}

impl SwitchableCredentials {
    pub fn new(api_key: &str) -> Self {
        Self {
            stored: Arc::new(Mutex::new(Some((api_key.into(), "rps_test".into())))),
            unreadable: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set(&self, api_key: &str) {
        self.set_with_secret(api_key, "rps_test");
    }

    pub fn set_with_secret(&self, api_key: &str, api_secret: &str) {
        *self.stored.lock().unwrap() = Some((api_key.into(), api_secret.into()));
    }

    /// As if the stored credentials were cleared, here or by another process.
    pub fn sign_out(&self) {
        *self.stored.lock().unwrap() = None;
    }

    /// As if the file could not be read right now (e.g. locked): every read fails until `false`.
    pub fn set_unreadable(&self, unreadable: bool) {
        self.unreadable.store(unreadable, Ordering::SeqCst);
    }

    pub fn loader(&self) -> CredentialLoader {
        let (stored, unreadable) = (self.stored.clone(), self.unreadable.clone());
        Arc::new(move || {
            if unreadable.load(Ordering::SeqCst) {
                return Err(io::Error::other("file locked"));
            }
            Ok(stored
                .lock()
                .unwrap()
                .as_ref()
                .map(|(key, secret)| auth_with_secret(key, secret)))
        })
    }
}

pub(crate) fn config(server_url: &str, credentials: CredentialLoader) -> EngineConfig {
    EngineConfig {
        server_url: server_url.to_string(),
        host_app: "Test Host".into(),
        host_version: "1.0".into(),
        credentials,
        jitter_seed: 7,
        list_merge: ListMergeConfig::default(),
        title_pointer: Some("/title".into()),
    }
}

/// Moves tokio time forward by `by`, then lets it run in real time again. A runtime paused for
/// a whole test auto-advances whenever it waits on I/O, which races real sockets.
pub(crate) async fn jump(by: Duration) {
    tokio::time::pause();
    tokio::time::advance(by).await;
    tokio::time::resume();
}

/// The next event `wanted` accepts, skipping others; fails after `WAIT`.
pub(crate) async fn wait_for(
    events: &mut mpsc::UnboundedReceiver<EngineEvent>,
    what: &str,
    wanted: impl Fn(&EngineEvent) -> bool,
) -> EngineEvent {
    tokio::time::timeout(WAIT, async {
        loop {
            let event = events.recv().await.expect("the engine is running");
            if wanted(&event) {
                return event;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Polls `check` every 20 ms of real time until it holds; fails after `WAIT`.
pub(crate) async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + WAIT;
    while !check().await {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
