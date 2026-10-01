mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use replicant_client::host::{self, Handle, HostEvent, Origin};
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use support::*;
use uuid::Uuid;

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

/// Like `wait_until`, but polls every millisecond so a child acts right after the condition.
fn wait_closely(within: Duration, mut check: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    check()
}

fn wait_for_file(path: &Path) {
    assert!(
        wait_until(Duration::from_secs(30), || path.exists()),
        "{} never appeared",
        path.display()
    );
}

/// The data dir, document and server a child process works with.
fn child_env() -> Option<(PathBuf, Uuid, Seed)> {
    let dir = std::env::var("TWO_PROCESS_DIR").ok()?;
    let doc_id = std::env::var("TWO_PROCESS_DOC").unwrap().parse().unwrap();
    let seed = Seed {
        server_url: std::env::var("TWO_PROCESS_URL").unwrap(),
        ..Seed::from_env()
    };
    Some((PathBuf::from(dir), doc_id, seed))
}

fn spawn_child_on(name: &str, dir: &Path, doc_id: Uuid, server_url: &str) -> std::process::Child {
    spawn_child(
        name,
        &[
            ("TWO_PROCESS_DIR", dir.display().to_string()),
            ("TWO_PROCESS_DOC", doc_id.to_string()),
            ("TWO_PROCESS_URL", server_url.to_string()),
        ],
    )
}

/// A read-only connection to the data dir's database, for watching the shared outbox.
fn open_outbox(handle: &Handle, dir: &Path) -> SqlitePool {
    handle
        .block_on(
            SqlitePoolOptions::new().max_connections(1).connect_with(
                SqliteConnectOptions::new()
                    .filename(dir.join("replicant.sqlite3"))
                    .read_only(true),
            ),
        )
        .unwrap()
}

/// How many of the document's outbox rows an upload has been sent for.
fn sent_rows(handle: &Handle, outbox: &SqlitePool, doc_id: Uuid) -> i64 {
    handle
        .block_on(
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM outbox WHERE doc_id = ? AND sent_upload_id IS NOT NULL",
            )
            .bind(doc_id.to_string())
            .fetch_one(outbox),
        )
        .unwrap()
}

fn create_uploaded(handle: &Handle, start: Value) -> Uuid {
    let doc_id = create(handle, start);
    wait_uploaded(handle);
    doc_id
}

/// Waits until `handle` has nothing left to upload and returns its content, after checking
/// that a third device's first sync sees the same content and nothing is parked.
fn settled_content(handle: &Handle, seed: &Seed, doc_id: Uuid) -> Value {
    wait_uploaded(handle);
    let local = content(handle, doc_id).unwrap();
    assert!(handle
        .block_on(handle.store().list_parked())
        .unwrap()
        .is_empty());
    let other = signed_in_dir(seed);
    let (third, _) = attach_synced(other.path(), seed);
    assert_eq!(
        content(&third, doc_id),
        Some(local.clone()),
        "the server holds another state"
    );
    close(third);
    local
}

fn recovered_count(handle: &Handle) -> usize {
    handle
        .block_on(handle.store().list_recovered())
        .unwrap()
        .len()
}

#[test]
#[ignore = "needs a v2 server: run through test/run_phoenix_interop_local.sh"]
fn two_online_processes_editing_one_document_converge() {
    let seed = Seed::from_env();
    let proxy = Proxy::start(&seed.server_url);
    let via_proxy = Seed {
        server_url: proxy.url.clone(),
        ..Seed::from_env()
    };
    let dir = signed_in_dir(&seed);
    let (handle, _) = attach_synced(dir.path(), &via_proxy);
    let doc_id = create_uploaded(&handle, json!({"title": "shared", "a": 0, "b": 0}));
    let outbox = open_outbox(&handle, dir.path());

    let child = spawn_child_on(
        "child_sets_b_once_a_is_sent",
        dir.path(),
        doc_id,
        &proxy.url,
    );
    wait_for_file(&dir.path().join("child_ready"));
    // Both processes' replies wait from here: one engine sends the edit of `a`, the child then
    // edits `b`, and the other engine uploads both rows on the base the server has replaced.
    proxy.set(Flow::HoldReplies);
    let mut edited = content(&handle, doc_id).unwrap();
    edited["a"] = 1.into();
    update(&handle, doc_id, edited);
    let raced = wait_until(LIVE, || sent_rows(&handle, &outbox, doc_id) == 2);
    // Otherwise both engines sent the edit of `a` before the child's edit existed.
    println!("second upload sent on a stale base: {raced}");
    proxy.set(Flow::Pass);
    assert!(child.wait_with_output().unwrap().status.success());

    assert_eq!(
        settled_content(&handle, &seed, doc_id),
        json!({"title": "shared", "a": 1, "b": 2})
    );
    assert_eq!(recovered_count(&handle), 0, "a kept copy for a clean merge");
    handle.block_on(outbox.close());
    close(handle);
}

#[test]
#[ignore = "child process of two_online_processes_editing_one_document_converge"]
fn child_sets_b_once_a_is_sent() {
    let Some((dir, doc_id, seed)) = child_env() else {
        return;
    };
    let (handle, _) = attach_synced(&dir, &seed);
    let outbox = open_outbox(&handle, &dir);
    std::fs::write(dir.join("child_ready"), "").unwrap();
    assert!(
        wait_closely(LIVE, || sent_rows(&handle, &outbox, doc_id) > 0),
        "the other process's edit was never sent"
    );
    let mut edited = content(&handle, doc_id).unwrap();
    assert_eq!(edited["a"], 1);
    edited["b"] = 2.into();
    update(&handle, doc_id, edited);
    handle.block_on(outbox.close());
    wait_uploaded(&handle);
    close(handle);
}

#[test]
#[ignore = "needs a v2 server: run through test/run_phoenix_interop_local.sh"]
fn two_online_processes_writing_the_same_document_end_with_one_server_state() {
    let seed = Seed::from_env();
    let dir = signed_in_dir(&seed);
    let (handle, _) = attach_synced(dir.path(), &seed);
    let doc_id = create_uploaded(&handle, json!({"title": "shared", "writer": "nobody"}));

    let child = spawn_child_on(
        "child_writes_whole_content_on_go",
        dir.path(),
        doc_id,
        &seed.server_url,
    );
    wait_for_file(&dir.path().join("child_ready"));
    std::fs::write(dir.path().join("go"), "").unwrap();
    update(
        &handle,
        doc_id,
        json!({"title": "shared", "writer": "parent"}),
    );
    assert!(child.wait_with_output().unwrap().status.success());

    let settled = settled_content(&handle, &seed, doc_id);
    assert!(
        settled["writer"] == "parent" || settled["writer"] == "child",
        "{settled}"
    );
    println!(
        "winner {} with {} kept copies",
        settled["writer"],
        recovered_count(&handle)
    );
    close(handle);
}

#[test]
#[ignore = "child process of two_online_processes_writing_the_same_document_end_with_one_server_state"]
fn child_writes_whole_content_on_go() {
    let Some((dir, doc_id, seed)) = child_env() else {
        return;
    };
    let (handle, _) = attach_synced(&dir, &seed);
    std::fs::write(dir.join("child_ready"), "").unwrap();
    assert!(wait_closely(LIVE, || dir.join("go").exists()));
    update(
        &handle,
        doc_id,
        json!({"title": "shared", "writer": "child"}),
    );
    wait_uploaded(&handle);
    close(handle);
}
