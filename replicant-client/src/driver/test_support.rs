//! Engine test fixtures: a seeded data dir, credential loaders, and helpers for tests that mix
//! real sockets with long timers.

use std::future::Future;
use std::path::PathBuf;
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
    JoinAuth {
        email: "a@b.c".into(),
        api_key: api_key.into(),
        api_secret: "rps_test".into(),
    }
}

pub(crate) fn credentials(api_key: &str) -> CredentialLoader {
    let api_key = api_key.to_string();
    Arc::new(move || Some(auth(&api_key)))
}

/// No stored credentials: the engine halts as not enrolled and never touches the network.
pub(crate) fn no_credentials() -> CredentialLoader {
    Arc::new(|| None)
}

/// Credentials a test can change while the engine runs.
#[derive(Clone)]
pub(crate) struct SwitchableCredentials(Arc<Mutex<String>>);

impl SwitchableCredentials {
    pub fn new(api_key: &str) -> Self {
        Self(Arc::new(Mutex::new(api_key.to_string())))
    }

    pub fn set(&self, api_key: &str) {
        *self.0.lock().unwrap() = api_key.to_string();
    }

    pub fn loader(&self) -> CredentialLoader {
        let api_key = self.0.clone();
        Arc::new(move || Some(auth(&api_key.lock().unwrap())))
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
