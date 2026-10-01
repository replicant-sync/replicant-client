//! The C API end to end, offline: nothing here needs a server.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use replicant_client::events::{EventOrigin, EventType};
use replicant_client::ffi::*;
use replicant_client::secret_store;
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
use uuid::Uuid;

const OFFLINE: &str = "ws://127.0.0.1:9";
const DB_FILE: &str = "replicant.sqlite3";

type Log = Mutex<Vec<String>>;

fn c(text: &str) -> CString {
    CString::new(text).unwrap()
}

fn read(value: *const c_char) -> String {
    if value.is_null() {
        "null".into()
    } else {
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    }
}

fn context(log: &Log) -> *mut c_void {
    log as *const Log as *mut c_void
}

fn log(context: *mut c_void) -> &'static Log {
    unsafe { &*(context as *const Log) }
}

extern "C" fn on_document(
    event_type: EventType,
    document_id: *const c_char,
    title: *const c_char,
    _content: *const c_char,
    _owner_id: *const c_char,
    _author_id: *const c_char,
    visibility: *const c_char,
    read_only: bool,
    origin: EventOrigin,
    context: *mut c_void,
) {
    log(context).lock().unwrap().push(format!(
        "{event_type:?} {} {} {} {read_only} {origin:?}",
        read(document_id),
        read(title),
        read(visibility)
    ));
}

extern "C" fn on_error(
    event_type: EventType,
    error_code: i32,
    error: *const c_char,
    document_id: *const c_char,
    scope: *const c_char,
    fatal: bool,
    recovered_id: i64,
    context: *mut c_void,
) {
    log(context).lock().unwrap().push(format!(
        "{event_type:?} {error_code} {} {} {} {fatal} {recovered_id}",
        read(error),
        read(document_id),
        read(scope)
    ));
}

fn create(dir: &Path, server_url: &str) -> (SyncResult, *mut Replicant) {
    create_with_lists(dir, server_url, 0, None)
}

fn create_with_lists(
    dir: &Path,
    server_url: &str,
    list_merge: i32,
    rules: Option<&str>,
) -> (SyncResult, *mut Replicant) {
    let rules = rules.map(c);
    create_with_config(dir, server_url, |config| {
        config.list_merge = list_merge;
        config.list_merge_rules_json = rules.as_ref().map_or(ptr::null(), |rules| rules.as_ptr());
    })
}

/// `create` with a config the test can change before the call.
fn create_with(
    dir: &Path,
    change: impl FnOnce(&mut ReplicantConfig),
) -> (SyncResult, *mut Replicant) {
    create_with_config(dir, OFFLINE, change)
}

fn create_with_config(
    dir: &Path,
    server_url: &str,
    change: impl FnOnce(&mut ReplicantConfig),
) -> (SyncResult, *mut Replicant) {
    let strings = [
        c(dir.to_str().unwrap()),
        c(DB_FILE),
        c(server_url),
        c("a@b.c"),
        c("Test Host"),
        c("1.0"),
    ];
    let mut config = ReplicantConfig {
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
    change(&mut config);
    let mut handle = ptr::null_mut();
    let result = unsafe { replicant_create(&config, &mut handle) };
    (result, handle)
}

fn open(dir: &Path) -> *mut Replicant {
    let (result, handle) = create(dir, OFFLINE);
    assert_eq!(result, SyncResult::Success);
    handle
}

fn close(handle: *mut Replicant) {
    assert!(unsafe { replicant_destroy_and_wait(handle, 10_000) });
}

fn take_string(out: *mut c_char) -> String {
    let text = read(out);
    unsafe { replicant_string_free(out) };
    text
}

fn create_doc(handle: *mut Replicant, json: &str) -> String {
    let content = c(json);
    let mut id = [0 as c_char; 37];
    assert_eq!(
        unsafe { replicant_create_document(handle, content.as_ptr(), id.as_mut_ptr()) },
        SyncResult::Success
    );
    read(id.as_ptr())
}

fn update(handle: *mut Replicant, id: &str, json: &str) -> SyncResult {
    unsafe { replicant_update_document(handle, c(id).as_ptr(), c(json).as_ptr()) }
}

fn get_doc(handle: *mut Replicant, id: &str) -> Result<Value, SyncResult> {
    let mut out = ptr::null_mut();
    match unsafe { replicant_get_document(handle, c(id).as_ptr(), &mut out) } {
        SyncResult::Success => Ok(serde_json::from_str(&take_string(out)).unwrap()),
        failed => Err(failed),
    }
}

fn json_out(call: impl FnOnce(*mut *mut c_char) -> SyncResult) -> Value {
    let mut out = ptr::null_mut();
    assert_eq!(call(&mut out), SyncResult::Success);
    serde_json::from_str(&take_string(out)).unwrap()
}

fn state(handle: *mut Replicant) -> ReplicantState {
    let mut state = ReplicantState {
        struct_size: std::mem::size_of::<ReplicantState>() as u32,
        connection: ReplicantConnection::Idle,
        sync: ReplicantSync::Idle,
        halt_reason: ReplicantHaltReason::None,
    };
    assert_eq!(
        unsafe { replicant_get_state(handle, &mut state) },
        SyncResult::Success
    );
    state
}

fn halted_not_enrolled(handle: *mut Replicant) -> bool {
    let state = state(handle);
    state.connection == ReplicantConnection::Halted
        && state.halt_reason == ReplicantHaltReason::NotEnrolled
}

/// Runs one statement against the data dir's database from outside the library.
fn sql(dir: &Path, statement: &str) {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let pool =
            SqlitePool::connect_with(SqliteConnectOptions::new().filename(dir.join(DB_FILE)))
                .await
                .unwrap();
        sqlx::query(statement).execute(&pool).await.unwrap();
        pool.close().await;
    });
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn pump_until(handle: *mut Replicant, log: &Log, what: &str, done: impl Fn(&[String]) -> bool) {
    wait_until(what, || {
        assert_eq!(
            unsafe { replicant_process_events(handle, ptr::null_mut()) },
            SyncResult::Success
        );
        done(&log.lock().unwrap())
    });
}

#[test]
fn a_second_create_on_the_same_data_dir_shares_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let first = open(dir.path());
    let second = open(dir.path());
    let id = create_doc(first, r#"{"title":"Shared"}"#);
    assert_eq!(get_doc(second, &id).unwrap()["title"], "Shared");
    close(first);
    close(second);
}

#[test]
fn create_with_a_different_server_for_an_open_data_dir_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let open_handle = open(dir.path());
    let (result, handle) = create(dir.path(), "ws://127.0.0.1:10");
    assert_eq!(result, SyncResult::ErrorConfigMismatch);
    assert!(handle.is_null());
    close(open_handle);
}

#[test]
fn create_refuses_a_full_or_malformed_list_merge_config() {
    let dir = tempfile::tempdir().unwrap();
    let refused: [(i32, Option<&str>); 5] = [
        (2, None),
        (7, None),
        (0, Some(r#"[{"path": "/pitches", "policy": "full"}]"#)),
        (0, Some(r#"[{"path": "pitches", "policy": "atomic"}]"#)),
        (0, Some("not json")),
    ];
    for (list_merge, rules) in refused {
        let (result, handle) = create_with_lists(dir.path(), OFFLINE, list_merge, rules);
        assert_eq!(
            result,
            SyncResult::ErrorInvalidInput,
            "{list_merge} {rules:?}"
        );
        assert!(handle.is_null());
    }
    let (result, handle) = create_with_lists(
        dir.path(),
        OFFLINE,
        1,
        Some(r#"[{"path": "/tunings/*/pitches", "policy": "append"}]"#),
    );
    assert_eq!(result, SyncResult::Success);
    let (mismatch, other) = create(dir.path(), OFFLINE);
    assert_eq!(
        mismatch,
        SyncResult::ErrorConfigMismatch,
        "one list policy per engine"
    );
    assert!(other.is_null());
    close(handle);
}

#[test]
fn create_on_a_database_from_a_newer_build_reports_newer_schema() {
    let dir = tempfile::tempdir().unwrap();
    close(open(dir.path()));
    sql(
        dir.path(),
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (99, 'from a newer build', 1, x'00', 0)",
    );
    let (result, handle) = create(dir.path(), OFFLINE);
    assert_eq!(result, SyncResult::ErrorNewerSchema);
    assert!(handle.is_null());
}

#[test]
fn documents_round_trip_through_writes_and_reads() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let first = create_doc(handle, r#"{"title":"A","n":1}"#);
    assert_eq!(
        update(handle, &first, r#"{"title":"A","n":2}"#),
        SyncResult::Success
    );
    let read_back = get_doc(handle, &first).unwrap();
    assert_eq!(read_back["content"]["n"], 2);
    assert_eq!(read_back["visibility"], "private");
    assert!(read_back["user_id"].is_string());
    let chosen = Uuid::new_v4().to_string();
    assert_eq!(
        unsafe { replicant_create_document_with_id(handle, c(&chosen).as_ptr(), c("{}").as_ptr()) },
        SyncResult::Success
    );
    let all = json_out(|out| unsafe { replicant_get_all_documents(handle, out) });
    assert_eq!(all.as_array().unwrap().len(), 2);
    let mut count = 0;
    assert_eq!(
        unsafe { replicant_count_documents(handle, &mut count) },
        SyncResult::Success
    );
    assert_eq!(count, 2);
    assert_eq!(
        unsafe { replicant_delete_document(handle, c(&chosen).as_ptr()) },
        SyncResult::Success
    );
    assert_eq!(get_doc(handle, &chosen), Err(SyncResult::ErrorNotFound));
    let live = json_out(|out| unsafe { replicant_get_all_document_ids(handle, false, out) });
    assert_eq!(live, json!([first]));
    let with_deleted = json_out(|out| unsafe { replicant_get_all_document_ids(handle, true, out) });
    assert_eq!(with_deleted.as_array().unwrap().len(), 2);
    close(handle);
}

#[test]
fn refused_writes_report_distinct_codes() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let missing = Uuid::new_v4().to_string();
    assert_eq!(update(handle, &missing, "{}"), SyncResult::ErrorNotFound);
    assert_eq!(
        unsafe { replicant_delete_document(handle, c(&missing).as_ptr()) },
        SyncResult::ErrorNotFound
    );
    let id = create_doc(handle, "{}");
    assert_eq!(
        unsafe { replicant_create_document_with_id(handle, c(&id).as_ptr(), c("{}").as_ptr()) },
        SyncResult::ErrorAlreadyExists
    );
    let theirs = Uuid::new_v4().to_string();
    sql(
        dir.path(),
        &format!(
            "INSERT INTO documents (id, user_id, content, created_at, updated_at) \
             VALUES ('{theirs}', '{}', '{{}}', 't', 't')",
            Uuid::new_v4()
        ),
    );
    assert_eq!(update(handle, &theirs, "{}"), SyncResult::ErrorNotWritable);
    assert_eq!(
        update(handle, "not-a-uuid", "{}"),
        SyncResult::ErrorInvalidInput
    );
    assert_eq!(
        update(handle, &id, "not json"),
        SyncResult::ErrorSerialization
    );
    close(handle);
}

#[test]
fn document_events_reach_the_callback_with_local_origin() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let seen: Log = Mutex::new(Vec::new());
    assert_eq!(
        unsafe {
            replicant_register_document_callback(handle, Some(on_document), context(&seen), -1)
        },
        SyncResult::Success
    );
    let id = create_doc(handle, r#"{"title":"Scale"}"#);
    pump_until(handle, &seen, "the created document", |lines| {
        lines.iter().any(|line| line.contains(&id))
    });
    assert_eq!(
        *seen.lock().unwrap(),
        vec![format!("DocumentChanged {id} Scale private false Local")]
    );
    close(handle);
}

#[test]
fn a_handle_without_credentials_reports_halted_not_enrolled() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let seen: Log = Mutex::new(Vec::new());
    assert_eq!(
        unsafe { replicant_register_error_callback(handle, Some(on_error), context(&seen)) },
        SyncResult::Success
    );
    wait_until("halted", || halted_not_enrolled(handle));
    pump_until(handle, &seen, "the not_enrolled error", |lines| {
        lines
            .iter()
            .any(|line| line == "SyncError 1003 not_enrolled null null true -1")
    });
    assert!(!unsafe { replicant_is_connected(handle) });
    close(handle);
}

#[test]
fn create_returns_with_the_state_the_stored_credentials_give() {
    let signed_out = tempfile::tempdir().unwrap();
    let first = open(signed_out.path());
    assert!(halted_not_enrolled(first));
    let second = open(signed_out.path());
    assert!(
        halted_not_enrolled(second),
        "a second handle shares the halt"
    );
    let seen: Log = Mutex::new(Vec::new());
    assert_eq!(
        unsafe { replicant_register_error_callback(first, Some(on_error), context(&seen)) },
        SyncResult::Success
    );
    pump_until(first, &seen, "the not_enrolled error", |lines| {
        !lines.is_empty()
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        unsafe { replicant_process_events(first, ptr::null_mut()) },
        SyncResult::Success
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["SyncError 1003 not_enrolled null null true -1".to_string()],
        "the start halt is reported once"
    );
    close(first);
    close(second);

    let signed_in = tempfile::tempdir().unwrap();
    secret_store::store(
        signed_in.path(),
        &secret_store::Credentials {
            api_key: "k1".into(),
            secret: "rps_test".into(),
            user_id: Uuid::new_v4(),
            email: Some("a@b.c".into()),
        },
    )
    .unwrap();
    let handle = open(signed_in.path());
    let dialling = state(handle);
    assert!(
        matches!(
            dialling.connection,
            ReplicantConnection::Connecting | ReplicantConnection::Disconnected
        ),
        "{dialling:?}"
    );
    assert_eq!(dialling.halt_reason, ReplicantHaltReason::None);
    close(handle);
}

#[test]
fn credentials_stored_by_0_6_need_the_config_email_to_sign_in() {
    let store_0_6 = |dir: &Path| {
        secret_store::store(
            dir,
            &secret_store::Credentials {
                api_key: "k1".into(),
                secret: "rps_test".into(),
                user_id: Uuid::new_v4(),
                email: None,
            },
        )
        .unwrap();
    };
    let without_email = tempfile::tempdir().unwrap();
    store_0_6(without_email.path());
    let (result, handle) = create_with(without_email.path(), |config| config.email = ptr::null());
    assert_eq!(result, SyncResult::Success);
    wait_until("halted", || halted_not_enrolled(handle));
    close(handle);

    let with_email = tempfile::tempdir().unwrap();
    store_0_6(with_email.path());
    let handle = open(with_email.path());
    wait_until("dialling", || {
        state(handle).connection != ReplicantConnection::Idle
    });
    assert!(!halted_not_enrolled(handle));
    close(handle);
}

#[test]
fn process_events_on_another_thread_is_refused_and_keeps_the_events() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let seen: Log = Mutex::new(Vec::new());
    assert_eq!(
        unsafe {
            replicant_register_document_callback(handle, Some(on_document), context(&seen), 1)
        },
        SyncResult::Success
    );
    let id = create_doc(handle, "{}");
    // Two change-log ticks: the event is queued by now.
    std::thread::sleep(Duration::from_millis(600));
    let address = handle as usize;
    std::thread::spawn(move || {
        let handle = address as *mut Replicant;
        assert_eq!(
            unsafe { replicant_process_events(handle, ptr::null_mut()) },
            SyncResult::ErrorWrongThread
        );
    })
    .join()
    .unwrap();
    let mut processed = 0;
    assert_eq!(
        unsafe { replicant_process_events(handle, &mut processed) },
        SyncResult::Success
    );
    assert!(processed >= 1);
    assert!(seen.lock().unwrap().iter().any(|line| line.contains(&id)));
    close(handle);
}

/// Holds the first callback until another thread has had its turn.
struct Gate {
    entered: std::sync::mpsc::Sender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    held: std::sync::atomic::AtomicBool,
}

extern "C" fn on_document_held(
    _event_type: EventType,
    _document_id: *const c_char,
    _title: *const c_char,
    _content: *const c_char,
    _owner_id: *const c_char,
    _author_id: *const c_char,
    _visibility: *const c_char,
    _read_only: bool,
    _origin: EventOrigin,
    context: *mut c_void,
) {
    let gate = unsafe { &*(context as *const Gate) };
    if !gate.held.swap(true, Ordering::SeqCst) {
        gate.entered.send(()).unwrap();
        gate.release.lock().unwrap().recv().unwrap();
    }
}

#[test]
fn process_events_on_another_thread_during_a_pump_is_refused_as_wrong_thread() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let gate = Gate {
        entered: entered_tx,
        release: Mutex::new(release_rx),
        held: std::sync::atomic::AtomicBool::new(false),
    };
    assert_eq!(
        unsafe {
            replicant_register_document_callback(
                handle,
                Some(on_document_held),
                &gate as *const Gate as *mut c_void,
                1,
            )
        },
        SyncResult::Success
    );
    create_doc(handle, "{}");
    let address = handle as usize;
    let other = std::thread::spawn(move || {
        entered_rx.recv().unwrap();
        let result =
            unsafe { replicant_process_events(address as *mut Replicant, ptr::null_mut()) };
        release_tx.send(()).unwrap();
        result
    });
    wait_until("the held callback", || {
        assert_eq!(
            unsafe { replicant_process_events(handle, ptr::null_mut()) },
            SyncResult::Success
        );
        gate.held.load(Ordering::SeqCst)
    });
    assert_eq!(other.join().unwrap(), SyncResult::ErrorWrongThread);
    assert_eq!(
        unsafe { replicant_process_events(handle, ptr::null_mut()) },
        SyncResult::Success,
        "the pump is free again"
    );
    close(handle);
}

#[test]
fn search_finds_configured_paths() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    assert_eq!(
        unsafe { replicant_configure_search(handle, c(r#"["$.body"]"#).as_ptr()) },
        SyncResult::Success
    );
    let music = create_doc(handle, r#"{"title":"Alpha","body":"music theory"}"#);
    create_doc(handle, r#"{"title":"Beta","body":"cooking"}"#);
    let found =
        json_out(|out| unsafe { replicant_search_documents(handle, c("music").as_ptr(), 0, out) });
    assert_eq!(found.as_array().unwrap().len(), 1);
    assert_eq!(found[0]["id"], music);
    assert_eq!(
        unsafe { replicant_rebuild_search_index(handle) },
        SyncResult::Success
    );
    close(handle);
}

#[test]
fn a_malformed_search_query_is_invalid_input() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let mut out = ptr::null_mut();
    assert_eq!(
        unsafe { replicant_search_documents(handle, c("a AND").as_ptr(), 0, &mut out) },
        SyncResult::ErrorInvalidInput
    );
    assert!(out.is_null());
    close(handle);
}

#[test]
fn a_search_limit_of_zero_returns_up_to_100() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    assert_eq!(
        unsafe { replicant_configure_search(handle, c(r#"["$.body"]"#).as_ptr()) },
        SyncResult::Success
    );
    for _ in 0..101 {
        create_doc(handle, r#"{"body":"music"}"#);
    }
    let search = |limit| {
        json_out(|out| unsafe {
            replicant_search_documents(handle, c("music").as_ptr(), limit, out)
        })
        .as_array()
        .unwrap()
        .len()
    };
    assert_eq!(search(0), 100);
    assert_eq!(search(3), 3);
    close(handle);
}

#[test]
fn count_pending_sync_counts_unsent_documents() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    create_doc(handle, "{}");
    create_doc(handle, "{}");
    let mut pending = 0;
    assert_eq!(
        unsafe { replicant_count_pending_sync(handle, &mut pending) },
        SyncResult::Success
    );
    assert_eq!(pending, 2);
    close(handle);
}

#[test]
fn a_claim_and_a_sign_out_move_the_engine_out_of_and_back_into_halted() {
    let (runtime, server, _) = claim_server(200, 1);
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    wait_until("halted as not enrolled", || halted_not_enrolled(handle));
    assert_eq!(claim(&server, dir.path()).0, SyncResult::Success);
    wait_until("dialling", || {
        state(handle).connection != ReplicantConnection::Halted
    });
    let data_dir = c(dir.path().to_str().unwrap());
    assert_eq!(
        unsafe { replicant_clear_credentials(data_dir.as_ptr()) },
        SyncResult::Success
    );
    wait_until("halted again", || halted_not_enrolled(handle));
    close(handle);
    runtime.block_on(server.verify());
}

#[test]
fn null_arguments_are_refused_without_crashing() {
    unsafe {
        assert!(replicant_destroy_and_wait(ptr::null_mut(), 0));
        replicant_destroy(ptr::null_mut());
        let mut count = 0;
        assert_eq!(
            replicant_count_documents(ptr::null_mut(), &mut count),
            SyncResult::ErrorInvalidInput
        );
        assert!(!replicant_is_connected(ptr::null_mut()));
        assert_eq!(
            replicant_create(ptr::null(), ptr::null_mut()),
            SyncResult::ErrorInvalidInput
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    // A null callback is not an error: it removes that kind's callback.
    let context = ptr::null_mut();
    unsafe {
        assert_eq!(
            replicant_register_document_callback(handle, None, context, -1),
            SyncResult::Success
        );
        assert_eq!(
            replicant_register_sync_callback(handle, None, context),
            SyncResult::Success
        );
        assert_eq!(
            replicant_register_error_callback(handle, None, context),
            SyncResult::Success
        );
        assert_eq!(
            replicant_register_connection_callback(handle, None, context),
            SyncResult::Success
        );
        assert_eq!(
            replicant_register_conflict_callback(handle, None, context),
            SyncResult::Success
        );
    }
    close(handle);
}

#[test]
fn a_failed_create_leaves_the_out_handle_null() {
    let stale = ptr::NonNull::<Replicant>::dangling().as_ptr();
    let mut handle = stale;
    assert_eq!(
        unsafe { replicant_create(ptr::null(), &mut handle) },
        SyncResult::ErrorInvalidInput
    );
    assert!(handle.is_null(), "null config");
    let dir = tempfile::tempdir().unwrap();
    let refused: [fn(&mut ReplicantConfig); 3] = [
        |config| config.struct_size = 4,
        |config| config.data_dir = ptr::null(),
        |config| config.list_merge = 7,
    ];
    for change in refused {
        let strings = [c(dir.path().to_str().unwrap()), c(DB_FILE), c(OFFLINE)];
        let mut config = ReplicantConfig {
            struct_size: std::mem::size_of::<ReplicantConfig>() as u32,
            data_dir: strings[0].as_ptr(),
            database_file: strings[1].as_ptr(),
            server_url: strings[2].as_ptr(),
            email: ptr::null(),
            host_app: strings[1].as_ptr(),
            host_version: strings[1].as_ptr(),
            list_merge: 0,
            list_merge_rules_json: ptr::null(),
        };
        change(&mut config);
        let mut handle = stale;
        assert_eq!(
            unsafe { replicant_create(&config, &mut handle) },
            SyncResult::ErrorInvalidInput
        );
        assert!(handle.is_null());
    }
}

#[test]
fn registering_again_replaces_the_callback_and_null_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let (first, second): (Log, Log) = (Mutex::new(Vec::new()), Mutex::new(Vec::new()));
    let register = |log: Option<&Log>| unsafe {
        replicant_register_document_callback(
            handle,
            log.map(|_| on_document as _),
            log.map_or(ptr::null_mut(), context),
            -1,
        )
    };
    assert_eq!(register(Some(&first)), SyncResult::Success);
    assert_eq!(register(Some(&second)), SyncResult::Success);
    let id = create_doc(handle, r#"{"title":"once"}"#);
    pump_until(handle, &second, "the write's event", |seen| {
        seen.iter().any(|line| line.contains(&id))
    });
    assert!(first.lock().unwrap().is_empty());
    assert_eq!(second.lock().unwrap().len(), 1);

    assert_eq!(register(None), SyncResult::Success);
    let removed = create_doc(handle, r#"{"title":"unheard"}"#);
    wait_until("the write's event to be taken", || {
        let mut processed = 0;
        assert_eq!(
            unsafe { replicant_process_events(handle, &mut processed) },
            SyncResult::Success
        );
        processed > 0
    });
    assert!(!second
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains(&removed)));
    close(handle);
}

/// What a mid-batch callback change needs: the handle, its own calls, and where to switch.
struct Switcher {
    handle: *mut Replicant,
    calls: AtomicUsize,
    next: Option<*const Log>,
}

/// On its first event, replaces itself with `on_document` into `next`, or removes itself.
extern "C" fn switch_on_first(
    _: EventType,
    _: *const c_char,
    _: *const c_char,
    _: *const c_char,
    _: *const c_char,
    _: *const c_char,
    _: *const c_char,
    _: bool,
    _: EventOrigin,
    context: *mut c_void,
) {
    let switcher = unsafe { &*(context as *const Switcher) };
    switcher.calls.fetch_add(1, Ordering::SeqCst);
    let (callback, next_context) = match switcher.next {
        Some(log) => (Some(on_document as _), log as *mut c_void),
        None => (None, ptr::null_mut()),
    };
    assert_eq!(
        unsafe {
            replicant_register_document_callback(switcher.handle, callback, next_context, -1)
        },
        SyncResult::Success
    );
}

/// Queues four document events, then pumps them in one batch.
fn pump_four_documents_in_one_batch(handle: *mut Replicant) {
    for n in 0..4 {
        create_doc(handle, &format!(r#"{{"n":{n}}}"#));
    }
    std::thread::sleep(Duration::from_millis(500));
    let mut processed = 0;
    assert_eq!(
        unsafe { replicant_process_events(handle, &mut processed) },
        SyncResult::Success
    );
    assert!(processed >= 4, "{processed}");
}

#[test]
fn a_callback_that_removes_itself_is_not_called_again_in_the_same_batch() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let switcher = Switcher {
        handle,
        calls: AtomicUsize::new(0),
        next: None,
    };
    let context = &switcher as *const Switcher as *mut c_void;
    assert_eq!(
        unsafe { replicant_register_document_callback(handle, Some(switch_on_first), context, -1) },
        SyncResult::Success
    );
    pump_four_documents_in_one_batch(handle);
    assert_eq!(switcher.calls.load(Ordering::SeqCst), 1);
    close(handle);
}

#[test]
fn a_callback_replaced_mid_batch_hands_the_rest_of_the_batch_to_the_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let rest: Log = Mutex::new(Vec::new());
    let switcher = Switcher {
        handle,
        calls: AtomicUsize::new(0),
        next: Some(&rest),
    };
    let context = &switcher as *const Switcher as *mut c_void;
    assert_eq!(
        unsafe { replicant_register_document_callback(handle, Some(switch_on_first), context, -1) },
        SyncResult::Success
    );
    pump_four_documents_in_one_batch(handle);
    assert_eq!(switcher.calls.load(Ordering::SeqCst), 1);
    assert_eq!(rest.lock().unwrap().len(), 3);
    close(handle);
}

#[test]
fn registering_off_the_bound_thread_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    assert_eq!(
        unsafe { replicant_register_document_callback(handle, None, ptr::null_mut(), -1) },
        SyncResult::Success
    );
    let handle_address = handle as usize;
    let result = std::thread::spawn(move || unsafe {
        replicant_register_error_callback(handle_address as *mut Replicant, None, ptr::null_mut())
    })
    .join()
    .unwrap();
    assert_eq!(result, SyncResult::ErrorWrongThread);
    close(handle);
}

/// A claim server answering `status` (with credentials on 200) and expecting `hits` calls.
fn claim_server(status: u16, hits: u64) -> (tokio::runtime::Runtime, wiremock::MockServer, Uuid) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let user_id = Uuid::new_v4();
    let server = runtime.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/enroll/claim"))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                "api_key": "rpa_claimed", "secret": "rps_claimed", "user_id": user_id
            })))
            .expect(hits)
            .mount(&server)
            .await;
        server
    });
    (runtime, server, user_id)
}

fn claim(server: &wiremock::MockServer, data_dir: &Path) -> (SyncResult, String) {
    let mut user_id = [0 as c_char; REPLICANT_USER_ID_LEN + 1];
    let result = unsafe {
        replicant_enroll_claim(
            c(&server.uri()).as_ptr(),
            c(data_dir.to_str().unwrap()).as_ptr(),
            c("a@b.c").as_ptr(),
            c("TOKEN").as_ptr(),
            user_id.as_mut_ptr(),
            user_id.len(),
        )
    };
    (result, read(user_id.as_ptr()))
}

#[test]
fn a_claim_stores_the_credentials_and_returns_only_the_user_id() {
    let (runtime, server, user_id) = claim_server(200, 1);
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        claim(&server, dir.path()),
        (SyncResult::Success, user_id.to_string())
    );
    let stored = secret_store::load(dir.path()).unwrap().unwrap();
    assert_eq!(
        (stored.api_key, stored.secret, stored.user_id, stored.email),
        (
            "rpa_claimed".to_string(),
            "rps_claimed".to_string(),
            user_id,
            Some("a@b.c".to_string())
        )
    );
    runtime.block_on(server.verify());
}

#[test]
fn a_claim_into_an_unwritable_data_dir_fails_without_spending_the_code() {
    let (runtime, server, _) = claim_server(200, 0);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a-file");
    std::fs::write(&file, "").unwrap();
    assert_eq!(
        claim(&server, &file.join("data")).0,
        SyncResult::ErrorDatabase
    );
    runtime.block_on(server.verify());
}

#[test]
fn a_rejected_code_has_its_own_result() {
    let (runtime, server, _) = claim_server(401, 1);
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(claim(&server, dir.path()).0, SyncResult::ErrorTokenRejected);
    assert!(secret_store::load(dir.path()).unwrap().is_none());
    runtime.block_on(server.verify());
}

#[test]
fn the_version_string_is_static() {
    let first = replicant_get_version();
    assert_eq!(first, replicant_get_version());
    assert_eq!(read(first), env!("CARGO_PKG_VERSION"));
}

#[test]
fn a_failed_read_leaves_its_out_pointer_null() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let mut out = ptr::dangling_mut::<c_char>();
    assert_eq!(
        unsafe {
            replicant_get_document(handle, c(&Uuid::new_v4().to_string()).as_ptr(), &mut out)
        },
        SyncResult::ErrorNotFound
    );
    assert!(out.is_null());
    close(handle);
}

#[test]
fn a_config_email_that_is_not_utf8_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let bad = CString::new(vec![0xFF, 0xFE]).unwrap();
    let (result, handle) = create_with(dir.path(), |config| config.email = bad.as_ptr());
    assert_eq!(result, SyncResult::ErrorInvalidInput);
    assert!(handle.is_null());
}

#[test]
fn a_config_or_state_smaller_than_this_version_is_refused() {
    assert_eq!(
        replicant_abi_version(),
        (REPLICANT_ABI_VERSION_MAJOR << 16) | REPLICANT_ABI_VERSION_MINOR
    );
    let dir = tempfile::tempdir().unwrap();
    let (result, handle) = create_with(dir.path(), |config| config.struct_size -= 4);
    assert_eq!(result, SyncResult::ErrorInvalidInput);
    assert!(handle.is_null());
    let handle = open(dir.path());
    let mut state = ReplicantState {
        struct_size: 3,
        ..state(handle)
    };
    assert_eq!(
        unsafe { replicant_get_state(handle, &mut state) },
        SyncResult::ErrorInvalidInput
    );
    close(handle);
}

/// A struct from a later header: this version's fields, then one it does not know.
#[repr(C)]
struct Later<T> {
    known: T,
    added: u64,
}

#[test]
fn a_config_or_state_from_a_later_version_is_accepted_and_its_new_fields_left_alone() {
    // The state's size comes back as the part this library filled.
    let dir = tempfile::tempdir().unwrap();
    let (result, handle) = create_with(dir.path(), |config| {
        config.struct_size = std::mem::size_of::<Later<ReplicantConfig>>() as u32
    });
    assert_eq!(result, SyncResult::Success);
    let mut later = Later {
        known: ReplicantState {
            struct_size: std::mem::size_of::<Later<ReplicantState>>() as u32,
            connection: ReplicantConnection::Stopped,
            sync: ReplicantSync::Live,
            halt_reason: ReplicantHaltReason::Other,
        },
        added: 0xDEAD_BEEF,
    };
    wait_until("halted as not enrolled", || {
        assert_eq!(
            unsafe { replicant_get_state(handle, &mut later.known) },
            SyncResult::Success
        );
        later.known.halt_reason == ReplicantHaltReason::NotEnrolled
    });
    assert_eq!(
        later.known.struct_size as usize,
        std::mem::size_of::<ReplicantState>()
    );
    assert_eq!(later.added, 0xDEAD_BEEF);
    close(handle);
}

fn committed_header() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("include/replicant.h"))
        .unwrap()
}

#[test]
fn the_committed_header_is_the_generated_one() {
    let generated = std::fs::read_to_string(env!("REPLICANT_GENERATED_HEADER")).unwrap();
    assert!(
        committed_header() == generated,
        "include/replicant.h is stale: copy {} over it",
        env!("REPLICANT_GENERATED_HEADER")
    );
}

#[test]
fn the_header_defines_only_replicant_macros() {
    for line in committed_header().lines() {
        if let Some(name) = line.strip_prefix("#define ") {
            assert!(name.starts_with("REPLICANT_"), "{line}");
        }
    }
}

#[test]
fn destroy_inside_a_callback_is_deferred_until_the_pump_returns() {
    static DESTROYS: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn destroy_on_first(
        _: EventType,
        _: *const c_char,
        _: *const c_char,
        _: *const c_char,
        _: *const c_char,
        _: *const c_char,
        _: *const c_char,
        _: bool,
        _: EventOrigin,
        context: *mut c_void,
    ) {
        DESTROYS.fetch_add(1, Ordering::SeqCst);
        unsafe { replicant_destroy(context as *mut Replicant) };
    }
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    assert_eq!(
        unsafe {
            replicant_register_document_callback(handle, Some(destroy_on_first), handle.cast(), 1)
        },
        SyncResult::Success
    );
    create_doc(handle, "{}");
    create_doc(handle, "{}");
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        unsafe { replicant_process_events(handle, ptr::null_mut()) },
        SyncResult::Success
    );
    // The not_enrolled halt is queued ahead of the documents, so count the callback's calls.
    assert_eq!(
        DESTROYS.load(Ordering::SeqCst),
        1,
        "the batch stops at the destroy; the handle is freed after it"
    );
}

fn keep_whole(dir: &Path, title: &str) {
    sql(
        dir,
        &format!(
            "INSERT INTO recovered (doc_id, content, reason, recovered_at) \
             VALUES ('{}', '{{\"title\":\"{title}\"}}', 'delete_wins', 100)",
            Uuid::new_v4()
        ),
    );
}

fn keep_field(dir: &Path, doc_id: &str, path: &str, local_value: Value) {
    let fields = json!([{"path": path, "local_value": local_value, "local_removed": false}]);
    sql(
        dir,
        &format!(
            "INSERT INTO recovered (doc_id, content, reason, recovered_at, fields) \
             VALUES ('{doc_id}', '{{}}', 'field_conflict', 200, '{fields}')"
        ),
    );
}

fn listed(handle: *mut Replicant) -> Value {
    json_out(|out| unsafe { replicant_list_recovered(handle, out) })
}

#[test]
fn kept_copies_are_listed_restored_and_dismissed() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let doc_id = create_doc(handle, r#"{"s":"theirs","k":1}"#);
    keep_whole(dir.path(), "Lost");
    keep_field(dir.path(), &doc_id, "/s", json!("mine"));
    let copies = listed(handle);
    assert_eq!(copies.as_array().unwrap().len(), 2);
    assert_eq!(copies[0]["reason"], "field_conflict", "newest first");
    assert_eq!(copies[0]["doc_id"], doc_id);
    assert_eq!(copies[0]["fields"][0]["path"], "/s");
    assert_eq!(copies[1]["title"], "Lost");
    assert!(copies[1]["fields"].is_null());
    let whole = copies[1]["recovered_id"].as_i64().unwrap();
    let field = copies[0]["recovered_id"].as_i64().unwrap();
    let mut restored = [0 as c_char; 37];
    assert_eq!(
        unsafe { replicant_restore_document(handle, whole, restored.as_mut_ptr()) },
        SyncResult::Success
    );
    assert_eq!(
        get_doc(handle, &read(restored.as_ptr())).unwrap()["content"],
        json!({"title": "Lost"})
    );
    assert_eq!(
        unsafe { replicant_dismiss_recovered(handle, field) },
        SyncResult::Success
    );
    assert_eq!(
        unsafe { replicant_dismiss_recovered(handle, field) },
        SyncResult::ErrorNotFound
    );
    assert_eq!(listed(handle), json!([]));
    close(handle);
}

#[test]
fn restore_fields_writes_the_kept_value_back() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let doc_id = create_doc(handle, r#"{"s":"theirs","k":1}"#);
    keep_field(dir.path(), &doc_id, "/s", json!("mine"));
    let field = listed(handle)[0]["recovered_id"].as_i64().unwrap();
    assert_eq!(
        unsafe { replicant_restore_fields(handle, field) },
        SyncResult::Success
    );
    assert_eq!(
        get_doc(handle, &doc_id).unwrap()["content"],
        json!({"s": "mine", "k": 1})
    );
    assert_eq!(
        unsafe { replicant_restore_fields(handle, field) },
        SyncResult::ErrorNotFound
    );
    close(handle);
}

#[test]
fn a_field_copy_of_a_deleted_document_is_reported_gone_and_restores_whole() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let doc_id = create_doc(handle, r#"{"s":"theirs"}"#);
    keep_field(dir.path(), &doc_id, "/s", json!("mine"));
    let field = listed(handle)[0]["recovered_id"].as_i64().unwrap();
    let id = c(&doc_id);
    assert_eq!(
        unsafe { replicant_delete_document(handle, id.as_ptr()) },
        SyncResult::Success
    );
    assert_eq!(
        unsafe { replicant_restore_fields(handle, field) },
        SyncResult::ErrorDocumentGone,
        "the document is gone, not the copy"
    );
    let mut restored = [0 as c_char; 37];
    assert_eq!(
        unsafe { replicant_restore_document(handle, field, restored.as_mut_ptr()) },
        SyncResult::Success,
        "any copy can come back as a new document"
    );
    assert_eq!(listed(handle), json!([]));
    close(handle);
}

#[test]
fn restore_fields_on_a_whole_document_copy_is_invalid_input_and_keeps_the_copy() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    keep_whole(dir.path(), "Lost");
    let whole = listed(handle)[0]["recovered_id"].as_i64().unwrap();
    assert_eq!(
        unsafe { replicant_restore_fields(handle, whole) },
        SyncResult::ErrorInvalidInput
    );
    assert_eq!(listed(handle)[0]["recovered_id"], whole);
    assert_eq!(
        unsafe { replicant_restore_fields(handle, whole + 1) },
        SyncResult::ErrorNotFound
    );
    close(handle);
}

#[test]
fn restore_fields_on_a_read_only_document_is_not_writable_and_keeps_the_copy() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let theirs = Uuid::new_v4().to_string();
    sql(
        dir.path(),
        &format!(
            "INSERT INTO documents (id, user_id, content, created_at, updated_at) \
             VALUES ('{theirs}', '{}', '{{}}', 't', 't')",
            Uuid::new_v4()
        ),
    );
    keep_field(dir.path(), &theirs, "/s", json!("mine"));
    let field = listed(handle)[0]["recovered_id"].as_i64().unwrap();
    assert_eq!(
        unsafe { replicant_restore_fields(handle, field) },
        SyncResult::ErrorNotWritable
    );
    assert_eq!(listed(handle)[0]["recovered_id"], field);
    close(handle);
}

#[test]
fn parked_documents_are_listed_with_their_code() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let doc_id = create_doc(handle, "{}");
    sql(
        dir.path(),
        &format!("UPDATE outbox SET parked_error = 'too_large' WHERE doc_id = '{doc_id}'"),
    );
    assert_eq!(
        json_out(|out| unsafe { replicant_list_parked(handle, out) }),
        json!([{"doc_id": doc_id, "code": "too_large"}])
    );
    let mut pending = 9;
    assert_eq!(
        unsafe { replicant_count_pending_sync(handle, &mut pending) },
        SyncResult::Success
    );
    assert_eq!(pending, 0, "parked is not syncing");
    close(handle);
}

#[test]
fn restoring_a_list_field_restores_the_whole_list() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    let doc_id = create_doc(handle, r#"{"items":["a","b","c"]}"#);
    keep_field(dir.path(), &doc_id, "/items", json!(["a", "B", "c"]));
    // The list changes shape after the conflict: a new element at the front.
    assert_eq!(
        update(handle, &doc_id, r#"{"items":["x","a","b","c"]}"#),
        SyncResult::Success
    );
    let field = listed(handle)[0]["recovered_id"].as_i64().unwrap();
    assert_eq!(
        unsafe { replicant_restore_fields(handle, field) },
        SyncResult::Success
    );
    assert_eq!(
        get_doc(handle, &doc_id).unwrap()["content"],
        json!({"items": ["a", "B", "c"]}),
        "list conflicts are whole-list, so the kept list comes back exactly"
    );
    close(handle);
}
