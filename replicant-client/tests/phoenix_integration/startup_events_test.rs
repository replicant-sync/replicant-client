//! Start-up connection and sync events, observed through the FFI.
//!
//! `replicant_create` reports one ConnectionSucceeded per real connect, once
//! the engine uses that connection, then SyncStarted and SyncCompleted for the
//! full sync that follows, whether the server is reachable at start-up or only
//! later.
//!
//! Gated behind `RUN_INTEGRATION_TESTS`; needs `SYNC_SERVER_URL`,
//! `REPLICANT_API_KEY`, `REPLICANT_API_SECRET` and `REPLICANT_TEST_USER_ID`.

use super::{
    canonical_user_id, serial, server_url, skip_if_no_server, temp_db_path, test_api_key,
    test_api_secret, TEST_EMAIL,
};
use crate::ffi_events::{
    create_engine, destroy_and_remove_db, pump_events_until, register_event_log, EventLog,
};
use replicant_client::events::EventType;
use replicant_client::ffi::{
    replicant_count_pending_sync, replicant_create_document, replicant_is_connected,
    replicant_process_events, Replicant, SyncResult,
};
use std::ffi::{c_char, CString};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use url::Url;

unsafe fn create_subject(db_path: &str, server_url: &str, log: &EventLog) -> *mut Replicant {
    let user_id = canonical_user_id().to_string();
    let engine = create_engine(
        db_path,
        server_url,
        TEST_EMAIL,
        &test_api_key(),
        &test_api_secret(),
        Some(&user_id),
    );
    assert!(!engine.is_null(), "replicant_create failed");
    register_event_log(engine, log);
    engine
}

/// Asserts one ConnectionSucceeded, SyncStarted and SyncCompleted, in that order.
fn assert_one_connect_then_sync(log: &EventLog) {
    let order = [
        EventType::ConnectionSucceeded,
        EventType::SyncStarted,
        EventType::SyncCompleted,
    ];
    for event_type in order {
        assert_eq!(
            log.count(event_type),
            1,
            "expected one {:?}: {:?}",
            event_type,
            log.events()
        );
    }
    let positions: Vec<_> = order.iter().map(|e| log.position(*e)).collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "expected {:?} in order: {:?}",
        order,
        log.events()
    );
}

/// Pumps without pausing until ConnectionSucceeded is delivered, and returns
/// whether the engine reported itself connected at that moment.
unsafe fn connected_when_announced(engine: *mut Replicant, log: &EventLog) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    while log.count(EventType::ConnectionSucceeded) == 0 {
        assert!(
            Instant::now() < deadline,
            "no ConnectionSucceeded: {:?}",
            log.events()
        );
        replicant_process_events(engine, std::ptr::null_mut());
        std::thread::yield_now();
    }
    replicant_is_connected(engine)
}

unsafe fn pending_count(engine: *mut Replicant) -> u64 {
    let mut count = 0;
    assert_eq!(
        replicant_count_pending_sync(engine, &mut count),
        SyncResult::Success
    );
    count
}

/// A TCP forwarder to the sync server. While offline it accepts and at once
/// closes each connection, so the server looks unreachable on its port.
struct ServerProxy {
    port: u16,
    online: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    streams: Arc<Mutex<Vec<TcpStream>>>,
    pumps: Arc<Mutex<Vec<JoinHandle<()>>>>,
    acceptor: Option<JoinHandle<()>>,
}

impl ServerProxy {
    fn start_offline() -> Self {
        let server = Url::parse(&server_url()).unwrap();
        let upstream = format!(
            "{}:{}",
            server.host_str().unwrap(),
            server.port_or_known_default().unwrap()
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let online = Arc::new(AtomicBool::new(false));
        let stopping = Arc::new(AtomicBool::new(false));
        let streams = Arc::new(Mutex::new(Vec::<TcpStream>::new()));
        let pumps = Arc::new(Mutex::new(Vec::new()));

        let acceptor = {
            let (online, stopping) = (online.clone(), stopping.clone());
            let (streams, pumps) = (streams.clone(), pumps.clone());
            std::thread::spawn(move || {
                for downstream in listener.incoming().flatten() {
                    if stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    if !online.load(Ordering::SeqCst) {
                        continue;
                    }
                    let Ok(upstream) = TcpStream::connect(&upstream) else {
                        continue;
                    };
                    let mut streams = streams.lock().unwrap();
                    streams.push(downstream.try_clone().unwrap());
                    streams.push(upstream.try_clone().unwrap());
                    let mut pumps = pumps.lock().unwrap();
                    pumps.push(Self::pump(
                        downstream.try_clone().unwrap(),
                        upstream.try_clone().unwrap(),
                    ));
                    pumps.push(Self::pump(upstream, downstream));
                }
            })
        };

        Self {
            port,
            online,
            stopping,
            streams,
            pumps,
            acceptor: Some(acceptor),
        }
    }

    fn pump(mut from: TcpStream, mut to: TcpStream) -> JoinHandle<()> {
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut from, &mut to);
            let _ = to.shutdown(Shutdown::Both);
            let _ = from.shutdown(Shutdown::Both);
        })
    }

    fn url(&self) -> String {
        format!("ws://127.0.0.1:{}/socket/websocket", self.port)
    }

    fn go_online(&self) {
        self.online.store(true, Ordering::SeqCst);
    }
}

impl Drop for ServerProxy {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        // Wakes the acceptor so it sees `stopping`.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(acceptor) = self.acceptor.take() {
            let _ = acceptor.join();
        }
        for stream in self.streams.lock().unwrap().iter() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        for pump in self.pumps.lock().unwrap().drain(..) {
            let _ = pump.join();
        }
    }
}

#[test]
#[serial]
fn online_startup_reports_connection_then_sync() {
    if skip_if_no_server() {
        eprintln!("skipping startup events test: RUN_INTEGRATION_TESTS not set");
        return;
    }
    std::fs::create_dir_all("databases").ok();
    let db_path = temp_db_path("startup_online");
    let log = EventLog::default();

    unsafe {
        let engine = create_subject(&db_path, &server_url(), &log);

        assert!(connected_when_announced(engine, &log));
        let synced = pump_events_until(engine, Duration::from_secs(30), || {
            log.count(EventType::SyncCompleted) > 0
        });
        assert!(
            synced,
            "no SyncCompleted from the server: {:?}",
            log.events()
        );
        assert_one_connect_then_sync(&log);

        // A new document uploads instead of queueing as pending.
        let content = CString::new(r#"{"title":"startup online"}"#).unwrap();
        let mut id = [0 as c_char; 37];
        assert_eq!(
            replicant_create_document(engine, content.as_ptr(), id.as_mut_ptr()),
            SyncResult::Success
        );
        let uploaded = pump_events_until(engine, Duration::from_secs(10), || {
            pending_count(engine) == 0
        });
        assert!(uploaded, "the document stayed pending");

        destroy_and_remove_db(engine, &db_path);
    }
}

#[test]
#[serial]
fn offline_startup_reports_connection_then_sync_after_reconnect() {
    if skip_if_no_server() {
        eprintln!("skipping startup events test: RUN_INTEGRATION_TESTS not set");
        return;
    }
    std::fs::create_dir_all("databases").ok();
    let db_path = temp_db_path("startup_reconnect");
    let proxy = ServerProxy::start_offline();
    let log = EventLog::default();

    unsafe {
        let engine = create_subject(&db_path, &proxy.url(), &log);

        // The start-up connect fails, then the reconnect monitor's first try does.
        let retried = pump_events_until(engine, Duration::from_secs(15), || {
            log.count(EventType::SyncError) >= 2
        });
        assert!(retried, "both connects should fail: {:?}", log.events());
        assert_eq!(log.count(EventType::ConnectionSucceeded), 0);
        assert_eq!(log.count(EventType::SyncCompleted), 0);

        proxy.go_online();

        assert!(connected_when_announced(engine, &log));
        let synced = pump_events_until(engine, Duration::from_secs(30), || {
            log.count(EventType::SyncCompleted) > 0
        });
        assert!(
            synced,
            "no SyncCompleted after reconnect: {:?}",
            log.events()
        );
        assert_one_connect_then_sync(&log);

        destroy_and_remove_db(engine, &db_path);
    }
    drop(proxy);
}
