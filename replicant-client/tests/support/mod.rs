use std::path::Path;
use std::time::{Duration, Instant};

use replicant_client::engine::list_merge::ListMergeConfig;
use replicant_client::host::{self, Handle, HostConfig, HostEvent};
use replicant_client::secret_store::{self, Credentials};
use serde_json::Value;
use uuid::Uuid;

pub const OFFLINE_URL: &str = "ws://127.0.0.1:9/socket/websocket";
const FIRST_SYNC: Duration = Duration::from_secs(20);
pub const LIVE: Duration = Duration::from_secs(10);
const CLOSE: Duration = Duration::from_secs(2);

pub fn config(data_dir: &Path, server_url: &str) -> HostConfig {
    HostConfig {
        data_dir: data_dir.to_path_buf(),
        database_file: "replicant.sqlite3".into(),
        server_url: server_url.into(),
        fallback_email: None,
        host_app: "replicant interop".into(),
        host_version: "3d".into(),
        list_merge: ListMergeConfig::default(),
        title_pointer: Some("/title".into()),
    }
}

pub fn wait_until(within: Duration, mut check: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    check()
}

/// Drops the events that do not match.
pub fn wait_event(
    handle: &Handle,
    within: Duration,
    mut pred: impl FnMut(&HostEvent) -> bool,
) -> Option<HostEvent> {
    let mut found = None;
    wait_until(within, || {
        found = handle.take_events().into_iter().find(|e| pred(e));
        found.is_some()
    });
    found
}

pub fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

pub struct Seed {
    pub api_key: String,
    pub secret: String,
    pub user_id: Uuid,
    pub email: String,
    pub server_url: String,
}

impl Seed {
    pub fn from_env() -> Seed {
        Seed {
            api_key: required_env("REPLICANT_API_KEY"),
            secret: required_env("REPLICANT_API_SECRET"),
            user_id: required_env("REPLICANT_TEST_USER_ID").parse().unwrap(),
            email: std::env::var("REPLICANT_TEST_EMAIL")
                .unwrap_or_else(|_| "integration-test@example.com".into()),
            server_url: required_env("SYNC_SERVER_URL"),
        }
    }
}

pub fn sign_in(data_dir: &Path, seed: &Seed) {
    secret_store::store(
        data_dir,
        &Credentials {
            api_key: seed.api_key.clone(),
            secret: seed.secret.clone(),
            user_id: seed.user_id,
            email: Some(seed.email.clone()),
        },
    )
    .unwrap();
}

pub fn signed_in_dir(seed: &Seed) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    sign_in(dir.path(), seed);
    dir
}

/// Attaches online and waits for the first `SyncCompleted`; returns every event up to it.
pub fn attach_synced(dir: &Path, seed: &Seed) -> (Handle, Vec<HostEvent>) {
    let handle = host::attach(config(dir, &seed.server_url)).unwrap();
    let mut events = Vec::new();
    let synced = wait_until(FIRST_SYNC, || {
        events.extend(handle.take_events());
        events.contains(&HostEvent::SyncCompleted)
    });
    assert!(synced, "no SyncCompleted within {FIRST_SYNC:?}: {events:?}");
    (handle, events)
}

pub fn content(handle: &Handle, doc_id: Uuid) -> Option<Value> {
    handle
        .block_on(handle.store().get_document(doc_id))
        .unwrap()
        .map(|document| document.content)
}

pub fn pending(handle: &Handle) -> u64 {
    handle
        .block_on(handle.store().count_pending_sync())
        .unwrap()
}

pub fn create(handle: &Handle, content: Value) -> Uuid {
    let doc_id = handle
        .block_on(handle.store().create_document(None, content))
        .unwrap();
    handle.notify_outbox();
    doc_id
}

pub fn update(handle: &Handle, doc_id: Uuid, content: Value) {
    handle
        .block_on(handle.store().update_document(doc_id, content))
        .unwrap();
    handle.notify_outbox();
}

pub fn wait_uploaded(handle: &Handle) {
    assert!(
        wait_until(LIVE, || pending(handle) == 0),
        "{} uploads still pending after {LIVE:?}",
        pending(handle)
    );
}

pub fn close(handle: Handle) {
    assert!(handle.close().wait(CLOSE));
}
