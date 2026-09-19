//! Shutdown / thread-lifetime tests for the C FFI (DEV-1118).
//!
//! `replicant_destroy` used to be a plain `Box::from_raw` drop. Dropping the
//! handle drops its tokio runtime (which cancels the background init task) and
//! drops the sqlite pools without closing them — and sqlx gives every sqlite
//! connection a dedicated OS thread that it spawns detached and never joins.
//! A connection still inside `establish()` therefore kept running Replicant
//! code after the handle was gone, which crashed hosts that unloaded the module
//! (an execute access violation on an unmapped image).
//!
//! These tests assert the contract that replaces it:
//!
//!  * `replicant_destroy` returns promptly — always, contended or not: a plugin
//!    host calls it on its UI thread. The teardown (closing both pools, joining
//!    the runtime's threads) happens on a Replicant thread afterwards, which is
//!    why a plugin must also pin its module.
//!  * `replicant_destroy_and_wait` is the opt-in wait, and returns `true` only
//!    once that teardown has actually finished, i.e. no Replicant thread is
//!    left.

use std::ffi::CString;
use std::os::raw::c_char;
use std::ptr;
use std::time::{Duration, Instant};

use replicant_client::database::sqlite_workers_started;
use replicant_client::ffi::{
    replicant_create, replicant_create_document, replicant_destroy, replicant_destroy_and_wait,
    Replicant, SyncResult,
};
use serial_test::serial;

/// Generous enough that reaching it means something is wrong, not slow.
const WAIT_MS: u32 = 30_000;

/// Live thread count of this process.
///
/// The tests in this file are `#[serial]` and each integration test file is its
/// own binary, so the only threads that move between the two measurements are
/// the ones Replicant starts.
/// Windows is where DEV-1118 was observed, so a probe that cannot answer must say
/// so loudly — it means these assertions are not checking anything on this run —
/// but it must not fail the run: this project's own runner has been without
/// `powershell` on PATH before, and that is not a defect in the library.
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

fn temp_db() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("shutdown.db");
    let url = format!("sqlite:{}?mode=rwc", path.to_string_lossy());
    (dir, url)
}

/// Creates a local-only client (empty api key => no socket, no server needed).
fn create_client(database_url: &str) -> *mut Replicant {
    let database_url = CString::new(database_url).unwrap();
    let server_url = CString::new("ws://127.0.0.1:1/ws").unwrap();
    let email = CString::new("shutdown-test@example.com").unwrap();
    let api_key = CString::new("").unwrap();
    let api_secret = CString::new("").unwrap();

    unsafe {
        replicant_create(
            database_url.as_ptr(),
            server_url.as_ptr(),
            email.as_ptr(),
            api_key.as_ptr(),
            api_secret.as_ptr(),
            ptr::null(),
        )
    }
}

/// Waits for the process's thread count to stop moving, then returns it.
///
/// Tests that use plain `replicant_destroy` deliberately leave a teardown
/// running, so a later test must not take its baseline while those threads are
/// still winding down — that inflates the baseline and makes the assertions
/// below meaningless in either direction.
///
/// Two equal samples is a heuristic: if it settles early the baseline is higher
/// than the true idle count, which can only weaken the assertions that use it,
/// never make them fail spuriously. (Those tests also wait their own detached
/// teardowns out, so there should be nothing left to settle.)
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

/// Asserts the process is back to `baseline` threads *now* — no settle window,
/// because "the wait returned true" is supposed to mean "the threads are gone".
fn assert_threads_settled(baseline: usize, label: &str) {
    let Some(now) = live_thread_count() else {
        eprintln!("{label}: thread enumeration unavailable on this platform, skipping");
        return;
    };
    assert!(
        now <= baseline,
        "{label}: {now} threads alive after destroy, baseline was {baseline} \
         ({} sqlite worker threads were started)",
        sqlite_workers_started()
    );
}

/// A zero timeout is the documented "don't wait" form: it must not block, and it
/// must report the teardown as unfinished rather than pretending otherwise.
///
/// Fails against the pre-fix client, which freed the handle and had nothing to
/// report, so every teardown looked complete.
#[test]
#[serial]
fn a_zero_timeout_does_not_wait_and_reports_unfinished() {
    let (_dir, url) = temp_db();
    let engine = create_client(&url);
    assert!(!engine.is_null(), "client creation failed");

    let started = Instant::now();
    let finished = unsafe { replicant_destroy_and_wait(engine, 0) };
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(500),
        "a zero timeout blocked for {elapsed:?}"
    );
    assert!(
        !finished,
        "a zero timeout claimed a teardown that had just started was complete"
    );

    // Leave nothing running in the temp dir.
    std::thread::sleep(Duration::from_millis(500));
}

/// Destroying a null handle stays a no-op, and destroy keeps its old signature.
#[test]
#[serial]
fn destroy_null_is_a_noop() {
    unsafe { replicant_destroy(ptr::null_mut()) };
    assert!(unsafe { replicant_destroy_and_wait(ptr::null_mut(), 0) });
}

/// Holds a sqlite write transaction on `url` for `hold`, so that any *write* by
/// another connection blocks on SQLITE_BUSY for that long while reads keep
/// working (the file is in WAL mode). That is the contention clap-validator's
/// parallel mode produces on the shared per-user database, and the state the
/// DEV-1118 faulting thread was in: a sqlx worker executing sqlite code inside
/// the plugin module, for seconds, with nothing joining it.
///
/// Returns a handle that has the lock by the time this returns, and the instant
/// the lock was taken — so a test can assert against the moment it is released
/// rather than against wall-clock margins that depend on setup cost.
fn hold_write_lock(url: &str, hold: Duration) -> (std::thread::JoinHandle<()>, Instant) {
    use sqlx::{ConnectOptions, Connection, Executor};
    use std::str::FromStr;

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let holder_url = url.to_string();
    let holder = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let mut conn = sqlx::sqlite::SqliteConnectOptions::from_str(&holder_url)
                .unwrap()
                .connect()
                .await
                .expect("holder connect");
            // IMMEDIATE takes the write lock now and keeps readers working.
            conn.execute("BEGIN IMMEDIATE").await.expect("holder lock");
            conn.execute("CREATE TABLE IF NOT EXISTS lock_me (x INTEGER)")
                .await
                .expect("holder write");
            ready_tx.send(()).ok();
            tokio::time::sleep(hold).await;
            conn.execute("ROLLBACK").await.ok();
            conn.close().await.ok();
        });
    });

    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("holder failed to take the write lock");
    (holder, Instant::now())
}

const LOCK_HOLD: Duration = Duration::from_secs(3);

/// The hazard itself, reproduced against sqlx directly: a query blocked on
/// SQLITE_BUSY runs on a thread sqlx spawned and never joins, so abandoning the
/// work by dropping the runtime — which is all the pre-fix `replicant_destroy`
/// did — leaves that thread executing code in this module.
///
/// This is a characterisation test of sqlx, not a regression test of Replicant:
/// it asserts that the hazard is real, so it passes against the pre-fix client
/// too, by construction. It is here to document why the teardown may not simply
/// drop the runtime. Every other test in this file fails against `0ea7565`.
#[test]
#[serial]
fn abandoning_a_blocked_query_leaves_a_thread_running() {
    let (_dir, url) = temp_db();
    // Migrate first, so the contention below is the only thing in play.
    assert!(unsafe { replicant_destroy_and_wait(create_client(&url), WAIT_MS) });

    let (holder, _lock_taken) = hold_write_lock(&url, LOCK_HOLD);

    {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let task_url = url.clone();
        let task = runtime.spawn(async move {
            let db = replicant_client::ClientDatabase::new(&task_url)
                .await
                .unwrap();
            sqlx::query("CREATE TABLE abandoned_write (x INTEGER)")
                .execute(&db.pool)
                .await
        });
        std::thread::sleep(Duration::from_millis(500));
        // The pre-fix teardown: drop the runtime, cancelling the task. Nothing
        // closes the pool, so nothing joins the sqlite worker thread.
        drop(task);
        drop(runtime);
    }

    // The write was issued by a cancelled task on a dropped runtime through a
    // dropped pool — and it still lands, once the lock frees. That is only
    // possible because the worker thread is alive and running sqlx and sqlite
    // code from this binary long after everything that owned it went away. A
    // plugin unloaded at this point would be executing unmapped memory.
    holder.join().unwrap();
    assert!(
        table_appears(&url, "abandoned_write", Duration::from_secs(10)),
        "the abandoned write never landed, so the worker thread had already exited"
    );
}

/// Polls `url` until `table` exists, up to `timeout`.
fn table_appears(url: &str, table: &str, timeout: Duration) -> bool {
    use sqlx::{ConnectOptions, Connection, Row};
    use std::str::FromStr;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let mut conn = sqlx::sqlite::SqliteConnectOptions::from_str(url)
            .unwrap()
            .connect()
            .await
            .expect("probe connect");

        let deadline = Instant::now() + timeout;
        loop {
            let found = sqlx::query(
                "SELECT COUNT(*) AS n FROM sqlite_master WHERE type = 'table' AND name = ?1",
            )
            .bind(table)
            .fetch_one(&mut conn)
            .await
            .ok()
            .and_then(|row| row.try_get::<i64, _>("n").ok())
            .unwrap_or(0);

            if found > 0 {
                conn.close().await.ok();
                return true;
            }
            if Instant::now() >= deadline {
                conn.close().await.ok();
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
}

/// A TCP listener that accepts connections and then never answers, so a client's
/// WebSocket handshake stalls until its own 10s connect timeout fires.
///
/// This is how the background init is made reliably slow. Sqlite contention is
/// not usable for that: SQLITE_BUSY sometimes comes back immediately instead of
/// going through sqlite's busy handler, so a held lock keeps the init busy only
/// sometimes. The stalled socket is deterministic, and it exercises the same
/// property — the teardown waits for the init task instead of cancelling it
/// mid-flight, which is what abandoned the sqlite worker in DEV-1118.
///
/// The listener thread outlives the test on purpose; take thread baselines after
/// calling this.
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

/// How long a stalled handshake keeps the background init busy: the client's own
/// WebSocket connect timeout (`CONNECT_TIMEOUT` in `websocket.rs`). Only the
/// order of magnitude matters here — the assertions below ask whether a call
/// waited for the init or returned straight away, so shortening the timeout in
/// `websocket.rs` means shortening this constant, nothing more.
const STALLED_INIT: Duration = Duration::from_secs(10);

/// Lower bound for "this call actually waited for the busy init". Well above a
/// prompt return (milliseconds) and well below the stall itself.
const WAITED_FOR_INIT: Duration = Duration::from_secs(2);

/// Creates a client that enrolls credentials, so its background init adopts the
/// identity and then connects a WebSocket — to `port`, which stalls.
fn create_stalling_client(database_url: &str, port: u16) -> *mut Replicant {
    let database_url = CString::new(database_url).unwrap();
    let server_url = CString::new(format!("ws://127.0.0.1:{port}/ws")).unwrap();
    let email = CString::new("shutdown-test@example.com").unwrap();
    let api_key = CString::new("rpa_test").unwrap();
    let api_secret = CString::new("rps_test").unwrap();
    let user_id = CString::new(uuid::Uuid::new_v4().to_string()).unwrap();

    unsafe {
        replicant_create(
            database_url.as_ptr(),
            server_url.as_ptr(),
            email.as_ptr(),
            api_key.as_ptr(),
            api_secret.as_ptr(),
            user_id.as_ptr(),
        )
    }
}

/// The contract for hosts: destroy a handle whose background init is still busy,
/// and destroy returns at once anyway. A plugin host calls it on its UI thread
/// during a scan or a project close and may not be stalled there.
///
/// This one guards the *new* contract: it fails against a destroy that tears
/// down synchronously, not against `0ea7565`, whose destroy returned at once
/// because it did nothing at all.
#[test]
#[serial]
fn destroy_returns_promptly_while_the_init_is_busy() {
    let (dir, url) = temp_db();
    let port = stalled_server();

    let engine = create_stalling_client(&url, port);
    assert!(!engine.is_null(), "client creation failed");

    let started = Instant::now();
    unsafe { replicant_destroy(engine) };
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(1),
        "destroy blocked for {elapsed:?} while the background init was busy"
    );
    // The init cannot have finished yet, so there really was work outstanding.
    assert!(
        started.elapsed() < STALLED_INIT,
        "the init had already finished, so destroy was not tested against a busy one"
    );

    // The teardown runs on for as long as the stalled handshake does. Wait it out
    // here — not because this test needs it, but so the next test can take a
    // thread baseline that is not polluted by this one's detached threads. (The
    // whole point of the contract is that the *caller* never has to do this.)
    std::thread::sleep(STALLED_INIT + Duration::from_secs(2));
    drop(dir);
}

/// The wait must report a teardown it could not finish, so a caller can tell
/// "clean" from "gave up" — the outcome is a return value, not just a log.
///
/// Fails against the pre-fix client, which reported nothing and freed the handle
/// while the init ran on.
#[test]
#[serial]
fn the_explicit_wait_reports_a_teardown_it_could_not_finish() {
    let (dir, url) = temp_db();
    let port = stalled_server();

    let engine = create_stalling_client(&url, port);
    assert!(!engine.is_null(), "client creation failed");

    // Far shorter than the stalled handshake, so the teardown cannot be done.
    let started = Instant::now();
    let finished = unsafe { replicant_destroy_and_wait(engine, 200) };
    let elapsed = started.elapsed();

    assert!(
        !finished,
        "the wait reported a clean teardown after only {elapsed:?}, while the \
         background init was still busy"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "a 200ms wait took {elapsed:?}"
    );

    // As above: wait the detached teardown out so it cannot pollute the next
    // test's thread baseline.
    std::thread::sleep(STALLED_INIT + Duration::from_secs(2));
    drop(dir);
}

/// The opt-in wait: it returns only once the teardown is actually complete, which
/// cannot be before the background init it waits for has finished. The full
/// lifecycle runs through it — create, write a document, tear down with the init
/// still busy — and when it reports success, no Replicant thread is left.
///
/// Fails against the pre-fix client, whose destroy returned in ~3ms with the
/// init's sqlite worker still executing code inside this module.
#[test]
#[serial]
fn the_explicit_wait_returns_only_after_the_teardown_is_complete() {
    let (_dir, url) = temp_db();
    let port = stalled_server();
    let workers_before = sqlite_workers_started();

    let engine = create_stalling_client(&url, port);
    assert!(!engine.is_null(), "client creation failed");

    // Local writes work while the init is still connecting.
    let content = CString::new(r#"{"title":"doc","body":"hello"}"#).unwrap();
    let mut out_id = [0 as c_char; 37];
    let result =
        unsafe { replicant_create_document(engine, content.as_ptr(), out_id.as_mut_ptr()) };
    assert_eq!(result, SyncResult::Success, "document creation failed");

    let started = Instant::now();
    let finished = unsafe { replicant_destroy_and_wait(engine, WAIT_MS) };
    let elapsed = started.elapsed();

    assert!(finished, "the wait gave up after {elapsed:?}");
    // Both pools were opened (the handle's own and the sync engine's), so the
    // teardown really did let the init task finish rather than cancel it.
    assert!(
        sqlite_workers_started() > workers_before + 1,
        "expected the background init to have opened its own pool; \
         workers started went {} -> {}",
        workers_before,
        sqlite_workers_started()
    );
    // It waited for the outstanding init rather than abandoning it: the stalled
    // handshake cannot complete before its own connect timeout.
    assert!(
        elapsed >= WAITED_FOR_INIT,
        "the wait returned after only {elapsed:?}, so it did not wait for the \
         busy background init"
    );
}

/// A client whose construction fails must leave no sqlite worker behind.
///
/// This is the failure mode the crash was reported against — parallel instances
/// whose migrations outlast sqlite's 5s busy timeout — reached on the background
/// init path, where no handle ever exists for anyone to destroy.
///
/// A guard, not a regression test: it passes pre-fix as well, because dropping a
/// pool drops its idle connections, which drops each worker's command channel,
/// which makes an *idle* worker notice and exit. What the explicit
/// `ClientDatabase::close()` on the failure path adds is that this is awaited and
/// deterministic rather than a race — a worker that is mid-statement or still
/// inside `establish()` ignores a dropped channel until it is done, and by then
/// the runtime may be gone and the module unloaded (DEV-1118).
#[test]
#[serial]
fn a_failed_client_construction_closes_its_pool() {
    let (_dir, url) = temp_db();
    // Held past the 5s busy timeout, so the migrations inside construction fail.
    let (holder, _lock_taken) = hold_write_lock(&url, Duration::from_secs(8));
    let Some(baseline) = quiesce_threads() else {
        eprintln!("thread enumeration unavailable on this platform, skipping");
        holder.join().unwrap();
        return;
    };

    {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(replicant_client::Client::new(
            &url,
            "ws://127.0.0.1:1/ws",
            "shutdown-test@example.com",
            "",
            "",
            None,
        ));
        assert!(
            result.is_err(),
            "expected construction to fail while the database is locked"
        );
        drop(runtime);
    }

    // Measured while the holder is still alive, so its own threads are in both
    // readings and cannot mask a leaked worker.
    assert_threads_settled(baseline, "a_failed_client_construction_closes_its_pool");
    holder.join().unwrap();
}

/// Re-instantiating immediately after a non-blocking destroy, which is what a
/// host does when it reloads a plugin: the outgoing instance's pools are still
/// open and its init may still be writing, and the new client must still come up.
///
/// The documented caveat (see `replicant_destroy` in replicant.h) is that this
/// overlap is sqlite contention, so a create can fail if it outlasts the 5s busy
/// timeout — this pins the uncontended case, where it must not.
#[test]
#[serial]
fn an_immediate_recreate_after_destroy_succeeds() {
    let (_dir, url) = temp_db();

    let first = create_client(&url);
    assert!(!first.is_null(), "first client creation failed");

    // No wait, no sleep: the teardown of the first is still in flight.
    unsafe { replicant_destroy(first) };
    let second = create_client(&url);
    assert!(
        !second.is_null(),
        "creating a client while the previous one was still tearing down failed"
    );

    assert!(
        unsafe { replicant_destroy_and_wait(second, WAIT_MS) },
        "teardown did not finish within {WAIT_MS}ms"
    );
}

/// When the wait reports success, the threads Replicant owns — its tokio runtime
/// and sqlx's per-connection workers — are gone.
///
/// Local-only (no credentials, so no socket): a timed-out WebSocket handshake can
/// leave threads inside the socket/TLS stack that Replicant neither owns nor can
/// join, and this assertion is about Replicant's own.
///
/// A characterisation test, not a regression guard: it passes against the pre-fix
/// client too, because uncontended the abandoned workers happened to exit anyway
/// — just not before destroy returned, and not at all when contended.
#[test]
#[serial]
fn a_waited_teardown_leaves_no_replicant_threads() {
    let (_dir, url) = temp_db();
    let Some(baseline) = quiesce_threads() else {
        eprintln!("thread enumeration unavailable on this platform, skipping");
        return;
    };

    let engine = create_client(&url);
    assert!(!engine.is_null(), "client creation failed");

    let content = CString::new(r#"{"title":"doc","body":"hello"}"#).unwrap();
    let mut out_id = [0 as c_char; 37];
    let result =
        unsafe { replicant_create_document(engine, content.as_ptr(), out_id.as_mut_ptr()) };
    assert_eq!(result, SyncResult::Success, "document creation failed");
    assert!(
        sqlite_workers_started() > 0,
        "no sqlite worker threads were started"
    );

    assert!(
        unsafe { replicant_destroy_and_wait(engine, WAIT_MS) },
        "teardown did not finish within {WAIT_MS}ms"
    );
    assert_threads_settled(baseline, "a_waited_teardown_leaves_no_replicant_threads");
}
