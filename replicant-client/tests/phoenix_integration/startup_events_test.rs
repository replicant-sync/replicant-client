//! Start-up connection and sync events, observed through the FFI (#45).
//!
//! `replicant_create` must report ConnectionSucceeded only for a real connect,
//! and SyncCompleted only once the server has answered the full sync, whether
//! the server is reachable at start-up or only later.
//!
//! Gated behind `RUN_INTEGRATION_TESTS`; needs `SYNC_SERVER_URL`,
//! `REPLICANT_API_KEY`, `REPLICANT_API_SECRET` and `REPLICANT_TEST_USER_ID`.

use super::{
    canonical_user_id, remove_temp_db, serial, server_url, skip_if_no_server, temp_db_path,
    test_api_key, test_api_secret, TEST_EMAIL,
};
use replicant_client::events::EventType;
use replicant_client::ffi::{
    replicant_create, replicant_destroy, replicant_is_connected, replicant_process_events,
    replicant_register_connection_callback, replicant_register_error_callback,
    replicant_register_sync_callback, Replicant, SyncResult,
};
use std::ffi::{c_char, c_void, CString};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use url::Url;

#[derive(Default)]
struct EventLog(Mutex<Vec<EventType>>);

impl EventLog {
    fn events(&self) -> Vec<EventType> {
        self.0.lock().unwrap().clone()
    }

    fn position(&self, event_type: EventType) -> Option<usize> {
        self.events().iter().position(|e| *e == event_type)
    }
}

extern "C" fn log_sync_event(event_type: EventType, _count: u64, context: *mut c_void) {
    let log = unsafe { &*(context as *const EventLog) };
    log.0.lock().unwrap().push(event_type);
}

extern "C" fn log_connection_event(
    event_type: EventType,
    _connected: bool,
    _attempt: u32,
    context: *mut c_void,
) {
    let log = unsafe { &*(context as *const EventLog) };
    log.0.lock().unwrap().push(event_type);
}

extern "C" fn log_error_event(
    event_type: EventType,
    _code: i32,
    _message: *const c_char,
    context: *mut c_void,
) {
    let log = unsafe { &*(context as *const EventLog) };
    log.0.lock().unwrap().push(event_type);
}

unsafe fn create_engine(db_url: &str, server_url: &str, log: &EventLog) -> *mut Replicant {
    let db_url = CString::new(db_url).unwrap();
    let server_url = CString::new(server_url).unwrap();
    let email = CString::new(TEST_EMAIL).unwrap();
    let api_key = CString::new(test_api_key()).unwrap();
    let api_secret = CString::new(test_api_secret()).unwrap();
    let user_id = CString::new(canonical_user_id().to_string()).unwrap();

    let engine = replicant_create(
        db_url.as_ptr(),
        server_url.as_ptr(),
        email.as_ptr(),
        api_key.as_ptr(),
        api_secret.as_ptr(),
        user_id.as_ptr(),
    );
    assert!(!engine.is_null(), "replicant_create failed");

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
    engine
}

/// Pumps events until `done` holds or `timeout` passes; returns whether `done` held.
unsafe fn pump_events_until(
    engine: *mut Replicant,
    log: &EventLog,
    timeout: Duration,
    done: impl Fn(&[EventType]) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        replicant_process_events(engine, std::ptr::null_mut());
        if done(&log.events()) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn assert_connected_then_synced(log: &EventLog) {
    let connected = log.position(EventType::ConnectionSucceeded);
    let synced = log.position(EventType::SyncCompleted);
    assert!(
        connected.is_some() && synced.is_some() && connected < synced,
        "expected ConnectionSucceeded before SyncCompleted: {:?}",
        log.events()
    );
}

/// A port that nothing listens on yet.
fn unused_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Starts forwarding `port` to the sync server, making the server reachable there.
fn start_proxy_to_server(port: u16) {
    let server = Url::parse(&server_url()).unwrap();
    let upstream = format!(
        "{}:{}",
        server.host_str().unwrap(),
        server.port_or_known_default().unwrap()
    );
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    std::thread::spawn(move || {
        for downstream in listener.incoming().flatten() {
            let Ok(upstream) = TcpStream::connect(&upstream) else {
                continue;
            };
            let (mut down_read, mut up_write) = (downstream.try_clone().unwrap(), upstream);
            let (mut up_read, mut down_write) = (up_write.try_clone().unwrap(), downstream);
            std::thread::spawn(move || std::io::copy(&mut down_read, &mut up_write));
            std::thread::spawn(move || std::io::copy(&mut up_read, &mut down_write));
        }
    });
}

#[test]
#[serial]
fn online_startup_reports_connection_then_sync() {
    if skip_if_no_server() {
        eprintln!("skipping startup events test: RUN_INTEGRATION_TESTS not set");
        return;
    }
    std::fs::create_dir_all("databases").ok();
    let db_file = temp_db_path("startup_online");
    let log = EventLog::default();

    unsafe {
        let engine = create_engine(&format!("sqlite:{}?mode=rwc", db_file), &server_url(), &log);

        let synced = pump_events_until(engine, &log, Duration::from_secs(30), |events| {
            events.contains(&EventType::SyncCompleted)
        });
        assert!(
            synced,
            "no SyncCompleted from the server: {:?}",
            log.events()
        );
        assert_connected_then_synced(&log);
        assert!(replicant_is_connected(engine));

        replicant_destroy(engine);
    }
    remove_temp_db(&db_file);
}

#[test]
#[serial]
fn offline_startup_reports_connection_then_sync_after_reconnect() {
    if skip_if_no_server() {
        eprintln!("skipping startup events test: RUN_INTEGRATION_TESTS not set");
        return;
    }
    std::fs::create_dir_all("databases").ok();
    let db_file = temp_db_path("startup_reconnect");
    let port = unused_port();
    let log = EventLog::default();

    unsafe {
        let engine = create_engine(
            &format!("sqlite:{}?mode=rwc", db_file),
            &format!("ws://127.0.0.1:{}/socket/websocket", port),
            &log,
        );

        let connect_failed = pump_events_until(engine, &log, Duration::from_secs(15), |events| {
            events.contains(&EventType::SyncError)
        });
        assert!(
            connect_failed,
            "the connect should fail: {:?}",
            log.events()
        );
        pump_events_until(engine, &log, Duration::from_secs(1), |_| false);
        assert_eq!(log.position(EventType::ConnectionSucceeded), None);
        assert_eq!(log.position(EventType::SyncCompleted), None);

        start_proxy_to_server(port);

        let synced = pump_events_until(engine, &log, Duration::from_secs(30), |events| {
            events.contains(&EventType::SyncCompleted)
        });
        assert!(
            synced,
            "no SyncCompleted after reconnect: {:?}",
            log.events()
        );
        assert_connected_then_synced(&log);
        assert!(replicant_is_connected(engine));

        replicant_destroy(engine);
    }
    remove_temp_db(&db_file);
}
