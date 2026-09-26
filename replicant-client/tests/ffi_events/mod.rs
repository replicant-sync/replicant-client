//! Engine setup and event capture shared by the FFI test suites.

use replicant_client::events::EventType;
use replicant_client::ffi::{
    replicant_create, replicant_destroy_and_wait, replicant_process_events,
    replicant_register_connection_callback, replicant_register_error_callback,
    replicant_register_sync_callback, Replicant, SyncResult,
};
use std::ffi::{c_char, c_void, CString};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Every sync, connection and error event an engine delivers, in order.
#[derive(Default)]
pub struct EventLog(Mutex<Vec<EventType>>);

impl EventLog {
    pub fn events(&self) -> Vec<EventType> {
        self.0.lock().unwrap().clone()
    }

    pub fn count(&self, event_type: EventType) -> usize {
        self.events().iter().filter(|e| **e == event_type).count()
    }

    pub fn position(&self, event_type: EventType) -> Option<usize> {
        self.events().iter().position(|e| *e == event_type)
    }

    fn push(context: *mut c_void, event_type: EventType) {
        let log = unsafe { &*(context as *const EventLog) };
        log.0.lock().unwrap().push(event_type);
    }
}

extern "C" fn log_sync_event(event_type: EventType, _count: u64, context: *mut c_void) {
    EventLog::push(context, event_type);
}

extern "C" fn log_connection_event(
    event_type: EventType,
    _connected: bool,
    _attempt: u32,
    context: *mut c_void,
) {
    EventLog::push(context, event_type);
}

extern "C" fn log_error_event(
    event_type: EventType,
    _code: i32,
    _message: *const c_char,
    context: *mut c_void,
) {
    EventLog::push(context, event_type);
}

/// Creates an engine on the sqlite file `db_path`. A `user_id` gives it an
/// adopted identity, so with an API key it syncs with `server_url`.
pub unsafe fn create_engine(
    db_path: &str,
    server_url: &str,
    email: &str,
    api_key: &str,
    api_secret: &str,
    user_id: Option<&str>,
) -> *mut Replicant {
    remove_db(db_path);
    let db_url = CString::new(format!("sqlite:{}?mode=rwc", db_path)).unwrap();
    let server_url = CString::new(server_url).unwrap();
    let email = CString::new(email).unwrap();
    let api_key = CString::new(api_key).unwrap();
    let api_secret = CString::new(api_secret).unwrap();
    let user_id = user_id.map(|id| CString::new(id).unwrap());

    replicant_create(
        db_url.as_ptr(),
        server_url.as_ptr(),
        email.as_ptr(),
        api_key.as_ptr(),
        api_secret.as_ptr(),
        user_id.as_ref().map_or(std::ptr::null(), |id| id.as_ptr()),
    )
}

pub unsafe fn register_event_log(engine: *mut Replicant, log: &EventLog) {
    let context = log as *const EventLog as *mut c_void;
    assert_eq!(
        replicant_register_sync_callback(engine, log_sync_event, context),
        SyncResult::Success
    );
    assert_eq!(
        replicant_register_connection_callback(engine, log_connection_event, context),
        SyncResult::Success
    );
    assert_eq!(
        replicant_register_error_callback(engine, log_error_event, context),
        SyncResult::Success
    );
}

/// Pumps events until `done` holds or `timeout` passes; returns whether `done` held.
pub unsafe fn pump_events_until(
    engine: *mut Replicant,
    timeout: Duration,
    done: impl Fn() -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        replicant_process_events(engine, std::ptr::null_mut());
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Destroys the engine, waits for its teardown, then deletes its database files.
pub unsafe fn destroy_and_remove_db(engine: *mut Replicant, db_path: &str) {
    assert!(
        replicant_destroy_and_wait(engine, 30_000),
        "engine teardown did not finish"
    );
    remove_db(db_path);
}

fn remove_db(db_path: &str) {
    for path in [
        db_path.to_string(),
        format!("{}-wal", db_path),
        format!("{}-shm", db_path),
    ] {
        let _ = std::fs::remove_file(path);
    }
}
