//! Plugin-host lifecycle through the C API: destroy never blocks the caller, and a finished
//! teardown leaves no thread behind. Serial: the thread counts are process-wide.

use std::ffi::{c_char, CString};
use std::path::Path;
use std::ptr;
use std::time::{Duration, Instant};

use replicant_client::ffi::*;
use replicant_client::{host, secret_store};
use serial_test::serial;

/// Live thread count of this process; `None` (said loudly) where it cannot be probed.
#[cfg(windows)]
fn live_thread_count() -> Option<usize> {
    let pid = std::process::id();
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(Get-Process -Id {pid}).Threads.Count"),
        ])
        .output();

    let out = match out {
        Ok(out) => out,
        Err(e) => {
            eprintln!(
                "!! THREAD-COUNT ASSERTIONS SKIPPED: could not run the probe \
                 (powershell: {e}) — the thread-lifetime guarantee is NOT checked \
                 on this run"
            );
            return None;
        }
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    match stdout.trim().parse() {
        Ok(count) => Some(count),
        Err(e) => {
            eprintln!(
                "!! THREAD-COUNT ASSERTIONS SKIPPED: the probe returned {:?} ({e}), \
                 stderr {:?} — the thread-lifetime guarantee is NOT checked on this run",
                stdout.trim(),
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn live_thread_count() -> Option<usize> {
    match std::fs::read_dir("/proc/self/task") {
        Ok(entries) => Some(entries.count()),
        Err(e) => {
            eprintln!(
                "!! THREAD-COUNT ASSERTIONS SKIPPED: could not read /proc/self/task \
                 ({e}) — the thread-lifetime guarantee is NOT checked on this run"
            );
            None
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn live_thread_count() -> Option<usize> {
    // No cheap dependency-free thread enumeration here; the timing assertions
    // still run, the thread-count ones are skipped.
    None
}

/// Waits for the process's thread count to stop moving (two equal samples), then returns it.
fn quiesce_threads() -> Option<usize> {
    let mut last = live_thread_count()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let now = live_thread_count()?;
        if now == last || Instant::now() >= deadline {
            return Some(now);
        }
        last = now;
    }
}

/// A TCP listener that accepts connections and never answers, so a WebSocket handshake stalls.
/// The listener thread outlives the test; take thread baselines after calling this.
fn stalled_server() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::Builder::new()
        .name("stalled-server".to_string())
        .spawn(move || {
            // Hold every accepted connection open, answering nothing.
            let mut held = Vec::new();
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => held.push(stream),
                    Err(_) => break,
                }
            }
        })
        .expect("spawn");
    port
}

fn c(text: &str) -> CString {
    CString::new(text).unwrap()
}

fn open(dir: &Path, server_url: &str) -> *mut Replicant {
    let strings = [
        c(dir.to_str().unwrap()),
        c("replicant.sqlite3"),
        c(server_url),
        c("a@b.c"),
        c("Test Host"),
        c("1.0"),
    ];
    let config = ReplicantConfig {
        struct_size: std::mem::size_of::<ReplicantConfig>() as u32,
        data_dir: strings[0].as_ptr(),
        database_file: strings[1].as_ptr(),
        server_url: strings[2].as_ptr(),
        email: strings[3].as_ptr(),
        host_app: strings[4].as_ptr(),
        host_version: strings[5].as_ptr(),
        list_merge: 0,
        list_merge_rules_json: ptr::null(),
    };
    let mut handle = ptr::null_mut();
    assert_eq!(
        unsafe { replicant_create(&config, &mut handle) },
        SyncResult::Success
    );
    handle
}

fn sign_in(dir: &Path) {
    secret_store::store(
        dir,
        &secret_store::Credentials {
            api_key: "rpa_k".into(),
            secret: "rps_s".into(),
            user_id: uuid::Uuid::new_v4(),
            email: Some("a@b.c".into()),
        },
    )
    .unwrap();
    host::credentials_changed(dir);
}

#[test]
#[serial]
fn create_destroy_cycles_leave_no_threads_behind() {
    let baseline = quiesce_threads();
    let dir = tempfile::tempdir().unwrap();
    for cycle in 0..20 {
        let handle = open(dir.path(), "ws://127.0.0.1:9");
        assert!(
            unsafe { replicant_destroy_and_wait(handle, 5_000) },
            "cycle {cycle}: the teardown did not finish"
        );
    }
    match (baseline, live_thread_count()) {
        (Some(baseline), Some(now)) => assert!(
            now <= baseline,
            "{now} threads alive after 20 cycles; baseline {baseline}"
        ),
        _ => eprintln!("thread enumeration unavailable here; only the waits were checked"),
    }
}

#[test]
#[serial]
fn destroy_returns_at_once_while_the_engine_dials_a_silent_server() {
    let port = stalled_server();
    let dir = tempfile::tempdir().unwrap();
    sign_in(dir.path());
    let handle = open(dir.path(), &format!("ws://127.0.0.1:{port}"));
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    unsafe { replicant_destroy(handle) };
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "destroy blocked for {:?}",
        started.elapsed()
    );
    // Let the detached teardown finish before the next test takes a thread baseline.
    std::thread::sleep(Duration::from_secs(3));
}

#[test]
#[serial]
fn destroy_and_wait_finishes_within_the_join_bound_while_dialling() {
    let port = stalled_server();
    let dir = tempfile::tempdir().unwrap();
    sign_in(dir.path());
    let handle = open(dir.path(), &format!("ws://127.0.0.1:{port}"));
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    assert!(unsafe { replicant_destroy_and_wait(handle, 10_000) });
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the teardown waited on the network: {:?}",
        started.elapsed()
    );
}

#[test]
#[serial]
fn destroy_of_one_of_two_handles_leaves_the_other_working() {
    let dir = tempfile::tempdir().unwrap();
    let staying = open(dir.path(), "ws://127.0.0.1:9");
    let leaving = open(dir.path(), "ws://127.0.0.1:9");
    let started = Instant::now();
    unsafe { replicant_destroy(leaving) };
    assert!(started.elapsed() < Duration::from_secs(1));
    let content = c("{}");
    let mut id = [0 as c_char; 37];
    assert_eq!(
        unsafe { replicant_create_document(staying, content.as_ptr(), id.as_mut_ptr()) },
        SyncResult::Success
    );
    assert!(unsafe { replicant_destroy_and_wait(staying, 10_000) });
}

/// Opens a one-connection pool on `path` and takes SQLite's write lock with `BEGIN IMMEDIATE`.
async fn hold_write_lock(path: &Path) -> sqlx::pool::PoolConnection<sqlx::Sqlite> {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(path))
        .await
        .unwrap();
    let mut connection = pool.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *connection)
        .await
        .unwrap();
    connection
}

async fn release(mut connection: sqlx::pool::PoolConnection<sqlx::Sqlite>) {
    sqlx::query("ROLLBACK")
        .execute(&mut *connection)
        .await
        .unwrap();
}

#[test]
#[serial]
fn destroy_returns_at_once_while_another_connection_holds_the_write_lock() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path(), "ws://127.0.0.1:9");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    // A second connection takes SQLite's write lock and keeps it while the handle goes.
    let lock_holder = runtime.block_on(hold_write_lock(&dir.path().join("replicant.sqlite3")));
    let started = Instant::now();
    unsafe { replicant_destroy(handle) };
    let elapsed = started.elapsed();
    // Released before the assert: a pooled connection dropped while panicking aborts the binary.
    runtime.block_on(release(lock_holder));
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}
