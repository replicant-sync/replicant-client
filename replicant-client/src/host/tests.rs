use std::cell::Cell;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::runtime::Runtime;

use super::fanout::FanOut;
use super::{
    attach, credential_loader, credentials_changed, lock, Handle, HostConfig, HostEvent, OpenError,
    Origin, REGISTRY,
};
use crate::driver::test_server::ScriptedServer;
use crate::engine::list_merge::{ListMergeConfig, ListMergePolicy};
use crate::engine::machine::{ConnectionView, HaltReason, SyncView};
use crate::secret_store::{self, Credentials};
use crate::store::test_support::{count, exec, ME};
use crate::store::Store;

/// Nothing listens here, so an engine with credentials keeps failing to connect.
const OFFLINE: &str = "ws://127.0.0.1:9";
const WAIT: Duration = Duration::from_secs(10);

type FanOutHook = Box<dyn FnOnce(&Mutex<FanOut>)>;

thread_local! {
    static AFTER_FANOUT_SPAWN: Cell<Option<FanOutHook>> = const { Cell::new(None) };
}

/// Runs this thread's hook once the new engine's fan-out task is running.
pub(super) fn after_fanout_spawn(fanout: &Mutex<FanOut>) {
    if let Some(hook) = AFTER_FANOUT_SPAWN.take() {
        hook(fanout);
    }
}

fn config(data_dir: &Path, server_url: &str) -> HostConfig {
    HostConfig {
        data_dir: data_dir.to_path_buf(),
        database_file: "replicant.sqlite3".into(),
        server_url: server_url.into(),
        fallback_email: Some("a@b.c".into()),
        host_app: "Test Host".into(),
        host_version: "1.0".into(),
        list_merge: ListMergeConfig::default(),
    }
}

fn sign_in(data_dir: &Path, api_key: &str) {
    secret_store::store(
        data_dir,
        &Credentials {
            api_key: api_key.into(),
            secret: "rps_test".into(),
            user_id: ME,
            email: None,
        },
    )
    .unwrap();
}

/// A scripted v2 server on its own runtime; keep both alive for the test.
fn live_server() -> (Runtime, ScriptedServer) {
    let runtime = Runtime::new().unwrap();
    let server = runtime.block_on(ScriptedServer::start(ME));
    (runtime, server)
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Takes `handle`'s events until `done` holds for everything taken so far.
fn events_until(
    handle: &Handle,
    what: &str,
    done: impl Fn(&[HostEvent]) -> bool,
) -> Vec<HostEvent> {
    let mut seen = Vec::new();
    wait_until(what, || {
        seen.extend(handle.take_events());
        done(&seen)
    });
    seen
}

fn halted_not_enrolled(handle: &Handle) -> bool {
    handle.state().connection == ConnectionView::Halted(HaltReason::NotEnrolled)
}

#[test]
fn two_handles_on_one_data_dir_share_one_engine() {
    let dir = tempfile::tempdir().unwrap();
    let first = attach(config(dir.path(), OFFLINE)).unwrap();
    let second = attach(config(dir.path(), OFFLINE)).unwrap();
    assert!(Arc::ptr_eq(&first.store(), &second.store()));
    assert!(first.close().wait(WAIT));
    assert!(second.close().wait(WAIT));
}

#[test]
fn a_different_server_for_an_open_data_dir_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let open = attach(config(dir.path(), OFFLINE)).unwrap();
    assert!(matches!(
        attach(config(dir.path(), "ws://127.0.0.1:10")),
        Err(OpenError::ConfigMismatch(_))
    ));
    let mut atomic_lists = config(dir.path(), OFFLINE);
    atomic_lists.list_merge.default = ListMergePolicy::Atomic;
    assert!(matches!(
        attach(atomic_lists),
        Err(OpenError::ConfigMismatch(_))
    ));
    assert!(open.close().wait(WAIT));
}

#[test]
fn the_last_close_stops_the_engine_and_a_new_attach_starts_another() {
    let dir = tempfile::tempdir().unwrap();
    let first = attach(config(dir.path(), OFFLINE)).unwrap();
    let first_instance = first.store().instance_id();
    assert!(first.close().wait(WAIT));
    let second = attach(config(dir.path(), OFFLINE)).unwrap();
    assert_ne!(second.store().instance_id(), first_instance, "a new engine");
    let readers = second.block_on(count(
        &second.store(),
        "SELECT COUNT(*) FROM change_log_readers",
    ));
    assert_eq!(
        readers, 1,
        "the stopped engine removed its change-log reader"
    );
    assert!(second.close().wait(WAIT));
}

#[test]
fn closing_one_of_two_handles_keeps_the_engine_running() {
    let dir = tempfile::tempdir().unwrap();
    let staying = attach(config(dir.path(), OFFLINE)).unwrap();
    let leaving = attach(config(dir.path(), OFFLINE)).unwrap();
    assert!(leaving.close().wait(WAIT), "nothing to wait for");
    assert_ne!(staying.state().connection, ConnectionView::Stopped);
    staying
        .block_on(staying.store().create_document(None, json!({"n": 1})))
        .unwrap();
    assert!(staying.close().wait(WAIT));
}

#[test]
fn a_late_handle_is_seeded_with_the_connection_it_joined() {
    let (_server_runtime, server) = live_server();
    let dir = tempfile::tempdir().unwrap();
    sign_in(dir.path(), "k1");
    let first = attach(config(dir.path(), &server.url)).unwrap();
    let seen = events_until(&first, "SyncCompleted", |events| {
        events.contains(&HostEvent::SyncCompleted)
    });
    assert_eq!(
        seen.iter()
            .filter(|event| **event == HostEvent::ConnectionSucceeded)
            .count(),
        1
    );
    let late = attach(config(dir.path(), &server.url)).unwrap();
    assert_eq!(
        late.take_events(),
        vec![
            HostEvent::ConnectionSucceeded,
            HostEvent::SyncStarted,
            HostEvent::SyncCompleted
        ]
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(late.take_events().is_empty(), "nothing is delivered twice");
    assert!(first.close().wait(WAIT));
    assert!(late.close().wait(WAIT));
}

#[test]
fn a_write_reaches_every_handle_once_as_local() {
    let dir = tempfile::tempdir().unwrap();
    let writer = attach(config(dir.path(), OFFLINE)).unwrap();
    let reader = attach(config(dir.path(), OFFLINE)).unwrap();
    let doc_id = writer
        .block_on(writer.store().create_document(None, json!({"title": "T"})))
        .unwrap();
    writer.notify_outbox();
    let is_change = |event: &HostEvent| matches!(event, HostEvent::DocumentChanged { document, .. } if document.id == doc_id);
    for handle in [&writer, &reader] {
        let mut seen = events_until(handle, "the change", |events| events.iter().any(is_change));
        std::thread::sleep(Duration::from_millis(600));
        seen.extend(handle.take_events());
        let changes: Vec<&HostEvent> = seen.iter().filter(|&event| is_change(event)).collect();
        assert_eq!(changes.len(), 1, "one event per document per pass");
        assert!(matches!(
            changes[0],
            HostEvent::DocumentChanged { origin: Origin::Local, document } if document.visibility == "private"
        ));
    }
    assert!(writer.close().wait(WAIT));
    assert!(reader.close().wait(WAIT));
}

#[test]
fn credentials_stored_after_attach_bring_the_engine_online() {
    let (_server_runtime, server) = live_server();
    let dir = tempfile::tempdir().unwrap();
    let handle = attach(config(dir.path(), &server.url)).unwrap();
    wait_until("halted as not enrolled", || halted_not_enrolled(&handle));
    sign_in(dir.path(), "k1");
    assert_eq!(credentials_changed(dir.path()), 1);
    wait_until("live", || handle.state().sync == SyncView::Live);
    assert_eq!(server.join_keys(), vec!["k1".to_string()]);
    assert!(handle.close().wait(WAIT));
}

#[test]
fn signing_out_halts_the_engine_and_it_never_joins_again() {
    let (_server_runtime, server) = live_server();
    let dir = tempfile::tempdir().unwrap();
    sign_in(dir.path(), "k1");
    let handle = attach(config(dir.path(), &server.url)).unwrap();
    wait_until("live", || handle.state().sync == SyncView::Live);
    secret_store::clear(dir.path()).unwrap();
    assert_eq!(credentials_changed(dir.path()), 1);
    wait_until("halted as not enrolled", || halted_not_enrolled(&handle));
    handle.reconnect();
    wait_until("one more dial, halted before its join", || {
        server.stats.upgrades() == 2 && halted_not_enrolled(&handle)
    });
    assert_eq!(server.join_keys(), vec!["k1".to_string()]);
    assert!(handle.close().wait(WAIT));
}

#[test]
fn a_database_from_a_newer_build_is_refused_as_newer_schema() {
    let dir = tempfile::tempdir().unwrap();
    assert!(attach(config(dir.path(), OFFLINE))
        .unwrap()
        .close()
        .wait(WAIT));
    let path = dir.path().join("replicant.sqlite3");
    Runtime::new().unwrap().block_on(async {
        let store = Store::open(&path).await.unwrap();
        exec(
            &store,
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (99, 'from a newer build', 1, x'00', 0)",
        )
        .await;
        store.close().await;
    });
    assert!(matches!(
        attach(config(dir.path(), OFFLINE)),
        Err(OpenError::NewerSchema)
    ));
}

#[test]
fn repeated_attach_and_close_cycles_finish_within_the_bound() {
    let dir = tempfile::tempdir().unwrap();
    for cycle in 0..20 {
        let started = Instant::now();
        let handle = attach(config(dir.path(), OFFLINE)).unwrap();
        assert!(
            handle.close().wait(Duration::from_secs(3)),
            "cycle {cycle} still stopping after {:?}",
            started.elapsed()
        );
    }
}

#[test]
fn the_first_handle_hears_the_engines_first_events() {
    let dir = tempfile::tempdir().unwrap();
    // `attach` returns only after the fan-out has published the engine's first event.
    AFTER_FANOUT_SPAWN.set(Some(Box::new(|fanout: &Mutex<FanOut>| {
        wait_until("the first published event", || lock(fanout).published > 0)
    })));
    let handle = attach(config(dir.path(), OFFLINE)).unwrap();
    let first_events = handle.take_events();
    assert!(
        matches!(first_events.first(), Some(HostEvent::SyncError { code, fatal: true, .. }) if code == "not_enrolled"),
        "{first_events:?}"
    );
    assert!(handle.close().wait(WAIT));
}

#[test]
fn credentials_for_one_data_dir_never_wait_for_an_engine_starting_on_another() {
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{ConnectOptions, Connection};

    let signed_in_dir = tempfile::tempdir().unwrap();
    let starting_dir = tempfile::tempdir().unwrap();
    assert!(attach(config(starting_dir.path(), OFFLINE))
        .unwrap()
        .close()
        .wait(WAIT));
    let starting_key = std::fs::canonicalize(starting_dir.path())
        .unwrap()
        .join("replicant.sqlite3");
    let lock_runtime = Runtime::new().unwrap();
    let mut write_lock = lock_runtime.block_on(async {
        let mut connection = SqliteConnectOptions::new()
            .filename(&starting_key)
            .connect()
            .await
            .unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut connection)
            .await
            .unwrap();
        connection
    });
    let starting = std::thread::spawn({
        let config = config(starting_dir.path(), OFFLINE);
        move || attach(config).map(Handle::close)
    });
    wait_until("the other engine is starting", || {
        lock(&REGISTRY)
            .get(&starting_key)
            .is_some_and(|slot| slot.0.try_lock().is_err())
    });

    let signed_in = attach(config(signed_in_dir.path(), OFFLINE)).unwrap();
    sign_in(signed_in_dir.path(), "k1");
    let told = std::thread::spawn({
        let dir = signed_in_dir.path().to_path_buf();
        move || credentials_changed(&dir)
    });
    wait_until("credentials_changed returns", || told.is_finished());
    assert_eq!(told.join().unwrap(), 1);
    assert!(
        !starting.is_finished(),
        "the other engine was still starting"
    );

    lock_runtime.block_on(async {
        sqlx::query("ROLLBACK")
            .execute(&mut write_lock)
            .await
            .unwrap();
        write_lock.close().await.unwrap();
    });
    assert!(starting.join().unwrap().unwrap().wait(WAIT));
    assert!(signed_in.close().wait(WAIT));
}

#[test]
fn a_full_policy_is_refused_as_config_even_with_an_engine_open() {
    let dir = tempfile::tempdir().unwrap();
    let open = attach(config(dir.path(), OFFLINE)).unwrap();
    let mut full = config(dir.path(), OFFLINE);
    full.list_merge.default = ListMergePolicy::Full;
    assert!(matches!(attach(full), Err(OpenError::Config(_))));
    let mut nested = config(dir.path(), OFFLINE);
    nested.database_file = "../replicant.sqlite3".into();
    assert!(matches!(attach(nested), Err(OpenError::Config(_))));
    assert!(open.close().wait(WAIT));
}

#[test]
fn the_first_join_announces_the_adopted_identity() {
    let (_runtime, server) = live_server();
    let dir = tempfile::tempdir().unwrap();
    sign_in(dir.path(), "k1");
    let handle = attach(config(dir.path(), &server.url)).unwrap();
    events_until(&handle, "IdentityAdopted", |seen| {
        seen.contains(&HostEvent::IdentityAdopted { user_id: ME })
    });
    assert_eq!(handle.block_on(handle.store().user_id()).unwrap(), ME);
    assert!(handle.close().wait(WAIT));
}

#[test]
fn the_join_email_comes_from_the_stored_credentials_before_the_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let with_fallback = credential_loader(dir.path().to_path_buf(), Some("fallback@x.io".into()));
    let without_fallback = credential_loader(dir.path().to_path_buf(), None);
    assert!(with_fallback().unwrap().is_none(), "nothing stored");
    sign_in(dir.path(), "k1");
    assert_eq!(with_fallback().unwrap().unwrap().email, "fallback@x.io");
    assert!(
        without_fallback().unwrap().is_none(),
        "no email anywhere: cannot sign a join"
    );
    secret_store::store(
        dir.path(),
        &Credentials {
            api_key: "k2".into(),
            secret: "rps_test".into(),
            user_id: ME,
            email: Some("stored@x.io".into()),
        },
    )
    .unwrap();
    assert_eq!(with_fallback().unwrap().unwrap().email, "stored@x.io");
    assert_eq!(without_fallback().unwrap().unwrap().api_key, "k2");
    std::fs::write(dir.path().join("credentials.enc"), b"torn").unwrap();
    assert!(with_fallback().is_err(), "unreadable is not signed out");
    std::fs::write(dir.path().join("credentials.enc"), [7u8; 40]).unwrap();
    assert!(with_fallback().is_err(), "undecryptable is not signed out");
}
