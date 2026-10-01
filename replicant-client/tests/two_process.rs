mod support;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use replicant_client::host::{self, HostEvent, Origin};
use support::*;

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

#[test]
fn an_offline_write_in_another_process_is_seen_within_a_second() {
    let dir = tempfile::tempdir().unwrap();
    let handle = host::attach(config(dir.path(), OFFLINE_URL)).unwrap();
    handle.take_events();

    let mut child = spawn_child(
        "child_writes_one_document",
        &[("TWO_PROCESS_DIR", dir.path().display().to_string())],
    );
    let seen = wait_event(&handle, Duration::from_secs(20), |e| {
        matches!(e, HostEvent::DocumentChanged { origin: Origin::OtherProcess, document }
            if document.content["title"] == "from the other process")
    });
    let seen_at = unix_ms();
    assert!(child.wait().unwrap().success());
    assert!(seen.is_some(), "the other process's write never arrived");
    let written_at: u128 = std::fs::read_to_string(dir.path().join("written_at"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The event can be seen before the child records its timestamp.
    let latency = seen_at.saturating_sub(written_at);
    println!("two-process latency {latency} ms");
    assert!(latency < 1000, "seen after {latency} ms");
    assert!(handle.close().wait(Duration::from_secs(2)));
}

/// Re-runs this test binary as a separate process that runs only the ignored test `name`.
fn spawn_child(name: &str, envs: &[(&str, String)]) -> std::process::Child {
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args([
        name,
        "--exact",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.spawn().unwrap()
}

#[test]
#[ignore = "child process of an_offline_write_in_another_process_is_seen_within_a_second"]
fn child_writes_one_document() {
    let Ok(dir) = std::env::var("TWO_PROCESS_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let handle = host::attach(config(&dir, OFFLINE_URL)).unwrap();
    let store = handle.store();
    handle
        .block_on(
            store.create_document(None, serde_json::json!({"title": "from the other process"})),
        )
        .unwrap();
    std::fs::write(dir.join("written_at"), unix_ms().to_string()).unwrap();
    assert!(handle.close().wait(Duration::from_secs(2)));
}
