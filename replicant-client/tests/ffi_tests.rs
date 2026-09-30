//! The C API end to end, offline: nothing here needs a server.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use replicant_client::events::{EventOrigin, EventType};
use replicant_client::ffi::*;
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
    fatal: bool,
    recovered_id: i64,
    context: *mut c_void,
) {
    log(context).lock().unwrap().push(format!(
        "{event_type:?} {error_code} {} {} {fatal} {recovered_id}",
        read(error),
        read(document_id)
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
        connection: ReplicantConnection::ConnectionIdle,
        sync: ReplicantSync::SyncIdle,
        halt_reason: ReplicantHaltReason::HaltNone,
    };
    assert_eq!(
        unsafe { replicant_get_state(handle, &mut state) },
        SyncResult::Success
    );
    state
}

fn halted_not_enrolled(handle: *mut Replicant) -> bool {
    let state = state(handle);
    state.connection == ReplicantConnection::ConnectionHalted
        && state.halt_reason == ReplicantHaltReason::HaltNotEnrolled
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
            .any(|line| line == "SyncError 1003 not_enrolled null true -1")
    });
    assert!(!unsafe { replicant_is_connected(handle) });
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
fn storing_and_clearing_credentials_move_the_engine_in_and_out_of_halted() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open(dir.path());
    wait_until("halted as not enrolled", || halted_not_enrolled(handle));
    let data_dir = c(dir.path().to_str().unwrap());
    assert_eq!(
        unsafe {
            replicant_store_credentials(
                data_dir.as_ptr(),
                c("a@b.c").as_ptr(),
                c("rpa_k").as_ptr(),
                c("rps_s").as_ptr(),
                c(&Uuid::new_v4().to_string()).as_ptr(),
            )
        },
        SyncResult::Success
    );
    wait_until("dialling", || {
        state(handle).connection != ReplicantConnection::ConnectionHalted
    });
    assert_eq!(
        unsafe { replicant_clear_credentials(data_dir.as_ptr()) },
        SyncResult::Success
    );
    wait_until("halted again", || halted_not_enrolled(handle));
    close(handle);
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
    let context = ptr::null_mut();
    unsafe {
        assert_eq!(
            replicant_register_document_callback(handle, None, context, -1),
            SyncResult::ErrorInvalidInput
        );
        assert_eq!(
            replicant_register_sync_callback(handle, None, context),
            SyncResult::ErrorInvalidInput
        );
        assert_eq!(
            replicant_register_error_callback(handle, None, context),
            SyncResult::ErrorInvalidInput
        );
        assert_eq!(
            replicant_register_connection_callback(handle, None, context),
            SyncResult::ErrorInvalidInput
        );
        assert_eq!(
            replicant_register_conflict_callback(handle, None, context),
            SyncResult::ErrorInvalidInput
        );
    }
    close(handle);
}

#[test]
fn a_config_or_state_with_an_unknown_struct_size_is_refused() {
    assert_eq!(replicant_abi_version(), REPLICANT_ABI_VERSION);
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
