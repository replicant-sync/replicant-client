use std::path::Path;
use std::time::{Duration, Instant};

use replicant_client::engine::list_merge::ListMergeConfig;
use replicant_client::host::{Handle, HostConfig, HostEvent};

pub const OFFLINE_URL: &str = "ws://127.0.0.1:9/socket/websocket";

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
