//! Integration tests for FFI event callbacks
//!
//! These tests verify that the FFI interface works correctly and that
//! callbacks are properly invoked when events occur.

use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use replicant_client::events::{EventOrigin, EventType};
use replicant_client::ffi::{
    replicant_count_documents, replicant_count_pending_sync, replicant_create_document,
    replicant_destroy, replicant_get_all_documents, replicant_get_document, replicant_get_user_id,
    replicant_is_connected, replicant_process_events, replicant_register_connection_callback,
    replicant_register_document_callback, replicant_register_error_callback,
    replicant_register_sync_callback, replicant_string_free, replicant_update_document, Replicant,
    SyncResult,
};

mod ffi_events;
use ffi_events::{
    create_engine, destroy_and_remove_db, pump_events_until, register_event_log, EventLog,
};

#[cfg(debug_assertions)]
use replicant_client::ffi_test::{replicant_emit_test_event, replicant_emit_test_event_burst};

/// Test data structure for capturing document callback invocations
#[derive(Debug, Default)]
struct DocumentCapture {
    call_count: AtomicUsize,
    last_event_type: Mutex<Option<EventType>>,
    last_document_id: Mutex<Option<String>>,
    last_title: Mutex<Option<String>>,
    last_content: Mutex<Option<String>>,
    last_user_id: Mutex<Option<String>>,
    last_author_name: Mutex<Option<String>>,
    last_visibility: Mutex<Option<String>>,
    last_origin: Mutex<Option<EventOrigin>>,
}

impl DocumentCapture {
    fn new() -> Self {
        Self::default()
    }

    fn reset(&self) {
        self.call_count.store(0, Ordering::SeqCst);
        *self.last_event_type.lock().unwrap() = None;
        *self.last_document_id.lock().unwrap() = None;
        *self.last_title.lock().unwrap() = None;
        *self.last_content.lock().unwrap() = None;
        *self.last_user_id.lock().unwrap() = None;
        *self.last_author_name.lock().unwrap() = None;
        *self.last_visibility.lock().unwrap() = None;
        *self.last_origin.lock().unwrap() = None;
    }
}

/// Test data structure for capturing sync callback invocations
#[derive(Debug, Default)]
struct SyncCapture {
    call_count: AtomicUsize,
    last_event_type: Mutex<Option<EventType>>,
    last_document_count: Mutex<u64>,
}

impl SyncCapture {
    fn new() -> Self {
        Self::default()
    }
}

/// Test data structure for capturing error callback invocations
#[derive(Debug, Default)]
struct ErrorCapture {
    call_count: AtomicUsize,
    last_error: Mutex<Option<String>>,
}

impl ErrorCapture {
    fn new() -> Self {
        Self::default()
    }
}

/// Test data structure for capturing connection callback invocations
#[derive(Debug, Default)]
struct ConnectionCapture {
    call_count: AtomicUsize,
    last_event_type: Mutex<Option<EventType>>,
    last_connected: Mutex<bool>,
    last_attempt: Mutex<u32>,
}

impl ConnectionCapture {
    fn new() -> Self {
        Self::default()
    }
}

extern "C" fn document_capture_callback(
    event_type: EventType,
    document_id: *const c_char,
    title: *const c_char,
    content: *const c_char,
    user_id: *const c_char,
    author_name: *const c_char,
    visibility: *const c_char,
    origin: EventOrigin,
    context: *mut c_void,
) {
    let capture = unsafe { &*(context as *const DocumentCapture) };
    *capture.last_origin.lock().unwrap() = Some(origin);

    capture.call_count.fetch_add(1, Ordering::SeqCst);
    *capture.last_event_type.lock().unwrap() = Some(event_type);

    if !document_id.is_null() {
        let doc_id = unsafe { CStr::from_ptr(document_id).to_string_lossy().to_string() };
        *capture.last_document_id.lock().unwrap() = Some(doc_id);
    }

    if !title.is_null() {
        let title_str = unsafe { CStr::from_ptr(title).to_string_lossy().to_string() };
        *capture.last_title.lock().unwrap() = Some(title_str);
    }

    if !content.is_null() {
        let content_str = unsafe { CStr::from_ptr(content).to_string_lossy().to_string() };
        *capture.last_content.lock().unwrap() = Some(content_str);
    }

    if !user_id.is_null() {
        let user_id_str = unsafe { CStr::from_ptr(user_id).to_string_lossy().to_string() };
        *capture.last_user_id.lock().unwrap() = Some(user_id_str);
    }

    if !author_name.is_null() {
        let author_name_str = unsafe { CStr::from_ptr(author_name).to_string_lossy().to_string() };
        *capture.last_author_name.lock().unwrap() = Some(author_name_str);
    }

    if !visibility.is_null() {
        let visibility_str = unsafe { CStr::from_ptr(visibility).to_string_lossy().to_string() };
        *capture.last_visibility.lock().unwrap() = Some(visibility_str);
    }
}

extern "C" fn sync_capture_callback(
    event_type: EventType,
    document_count: u64,
    context: *mut c_void,
) {
    let capture = unsafe { &*(context as *const SyncCapture) };
    capture.call_count.fetch_add(1, Ordering::SeqCst);
    *capture.last_event_type.lock().unwrap() = Some(event_type);
    *capture.last_document_count.lock().unwrap() = document_count;
}

extern "C" fn error_capture_callback(
    _event_type: EventType,
    _error_code: i32,
    error: *const c_char,
    context: *mut c_void,
) {
    let capture = unsafe { &*(context as *const ErrorCapture) };
    capture.call_count.fetch_add(1, Ordering::SeqCst);

    if !error.is_null() {
        let error_str = unsafe { CStr::from_ptr(error).to_string_lossy().to_string() };
        *capture.last_error.lock().unwrap() = Some(error_str);
    }
}

extern "C" fn connection_capture_callback(
    event_type: EventType,
    connected: bool,
    attempt_number: u32,
    context: *mut c_void,
) {
    let capture = unsafe { &*(context as *const ConnectionCapture) };
    capture.call_count.fetch_add(1, Ordering::SeqCst);
    *capture.last_event_type.lock().unwrap() = Some(event_type);
    *capture.last_connected.lock().unwrap() = connected;
    *capture.last_attempt.lock().unwrap() = attempt_number;
}

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique database path for one engine in this test process.
fn unique_db_path() -> String {
    let unique_id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!(
        "/tmp/sync_client_test_{}_{}.db",
        std::process::id(),
        unique_id
    )
}

/// Creates an engine on a fresh database; see `ffi_events::create_engine`.
unsafe fn create_test_engine_with(
    db_path: &str,
    server_url: &str,
    user_id: Option<&str>,
) -> *mut Replicant {
    create_engine(
        db_path,
        server_url,
        "test-user@example.com",
        "rpa_test_api_key_example_12345",
        "rps_test_api_secret_example_67890",
        user_id,
    )
}

/// Creates a local-only engine (no identity, so it never connects).
unsafe fn create_test_engine() -> *mut Replicant {
    create_test_engine_with(&unique_db_path(), "ws://localhost:8080/ws", None)
}

#[test]
fn test_ffi_callback_registration() {
    unsafe {
        let engine = create_test_engine();
        assert!(!engine.is_null(), "Failed to create sync engine");

        let capture = DocumentCapture::new();

        // Register callback for all document events
        let result = replicant_register_document_callback(
            engine,
            document_capture_callback,
            &capture as *const DocumentCapture as *mut c_void,
            -1, // All document events
        );

        assert_eq!(result, SyncResult::Success);

        replicant_destroy(engine);
    }
}

#[test]
fn test_ffi_callback_with_document_creation() {
    unsafe {
        let engine = create_test_engine();
        assert!(!engine.is_null(), "Failed to create sync engine");

        let capture = DocumentCapture::new();

        // Register callback for document events
        let result = replicant_register_document_callback(
            engine,
            document_capture_callback,
            &capture as *const DocumentCapture as *mut c_void,
            0, // DocumentCreated
        );
        assert_eq!(result, SyncResult::Success);

        // Create a document (this should trigger the callback in offline mode)
        let content =
            CString::new(r#"{"title":"Test Document","content":"test data","type":"note"}"#)
                .unwrap();
        let mut doc_id = [0u8; 37]; // UUID string + null terminator

        let create_result =
            replicant_create_document(engine, content.as_ptr(), doc_id.as_mut_ptr() as *mut i8);

        assert_eq!(create_result, SyncResult::Success);

        // Process events to trigger callbacks
        replicant_process_events(engine, ptr::null_mut());

        // Verify callback was called
        assert_eq!(capture.call_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            *capture.last_event_type.lock().unwrap(),
            Some(EventType::DocumentCreated)
        );

        let captured_title = capture.last_title.lock().unwrap();
        assert_eq!(captured_title.as_ref().unwrap(), "Test Document");

        let captured_content = capture.last_content.lock().unwrap();
        assert!(captured_content.is_some());
        assert!(captured_content.as_ref().unwrap().contains("test data"));

        // Documents created offline are always assigned an owner user_id, so this
        // must arrive on the callback. author_name/visibility are not seeded by
        // replicant_create_document, so they merely need to arrive without crashing.
        let captured_user_id = capture.last_user_id.lock().unwrap();
        assert!(captured_user_id.is_some());

        // The host created this document itself, so the event must say so;
        // otherwise a consumer cannot tell it apart from a remote change.
        assert_eq!(
            *capture.last_origin.lock().unwrap(),
            Some(EventOrigin::Local)
        );

        // Verify replicant_get_user_id returns the same engine-frozen user UUID
        let mut out_user_id: *mut c_char = ptr::null_mut();
        let get_user_id_result = replicant_get_user_id(engine, &mut out_user_id);
        assert_eq!(get_user_id_result, SyncResult::Success);
        assert!(!out_user_id.is_null());
        let engine_user_id = CStr::from_ptr(out_user_id).to_string_lossy().to_string();
        assert_eq!(engine_user_id, captured_user_id.as_ref().unwrap().clone());
        replicant_string_free(out_user_id);

        replicant_destroy(engine);
    }
}

#[test]
fn test_ffi_callback_filtering() {
    unsafe {
        let engine = create_test_engine();
        assert!(!engine.is_null(), "Failed to create sync engine");

        let created_capture = DocumentCapture::new();
        let updated_capture = DocumentCapture::new();

        // Register callback only for created events
        let result1 = replicant_register_document_callback(
            engine,
            document_capture_callback,
            &created_capture as *const DocumentCapture as *mut c_void,
            0, // DocumentCreated
        );
        assert_eq!(result1, SyncResult::Success);

        // Register callback only for updated events
        let result2 = replicant_register_document_callback(
            engine,
            document_capture_callback,
            &updated_capture as *const DocumentCapture as *mut c_void,
            1, // DocumentUpdated
        );
        assert_eq!(result2, SyncResult::Success);

        #[cfg(debug_assertions)]
        {
            // Test with debug event emission
            replicant_emit_test_event(engine, 0); // DocumentCreated
            replicant_process_events(engine, ptr::null_mut());

            assert_eq!(created_capture.call_count.load(Ordering::SeqCst), 1);
            assert_eq!(updated_capture.call_count.load(Ordering::SeqCst), 0);

            replicant_emit_test_event(engine, 1); // DocumentUpdated
            replicant_process_events(engine, ptr::null_mut());

            assert_eq!(created_capture.call_count.load(Ordering::SeqCst), 1);
            assert_eq!(updated_capture.call_count.load(Ordering::SeqCst), 1);

            replicant_emit_test_event(engine, 2); // DocumentDeleted (neither should trigger)
            replicant_process_events(engine, ptr::null_mut());

            assert_eq!(created_capture.call_count.load(Ordering::SeqCst), 1);
            assert_eq!(updated_capture.call_count.load(Ordering::SeqCst), 1);
        }

        replicant_destroy(engine);
    }
}

#[test]
fn test_ffi_multiple_callbacks() {
    unsafe {
        let engine = create_test_engine();
        assert!(!engine.is_null(), "Failed to create sync engine");

        let capture1 = SyncCapture::new();
        let capture2 = SyncCapture::new();
        let capture3 = SyncCapture::new();

        // Register three callbacks for sync events
        replicant_register_sync_callback(
            engine,
            sync_capture_callback,
            &capture1 as *const SyncCapture as *mut c_void,
        );

        replicant_register_sync_callback(
            engine,
            sync_capture_callback,
            &capture2 as *const SyncCapture as *mut c_void,
        );

        replicant_register_sync_callback(
            engine,
            sync_capture_callback,
            &capture3 as *const SyncCapture as *mut c_void,
        );

        #[cfg(debug_assertions)]
        {
            // Emit a test event
            replicant_emit_test_event(engine, 3); // SyncStarted
            replicant_process_events(engine, ptr::null_mut());

            // All three should have been called
            assert_eq!(capture1.call_count.load(Ordering::SeqCst), 1);
            assert_eq!(capture2.call_count.load(Ordering::SeqCst), 1);
            assert_eq!(capture3.call_count.load(Ordering::SeqCst), 1);

            assert_eq!(
                *capture1.last_event_type.lock().unwrap(),
                Some(EventType::SyncStarted)
            );
            assert_eq!(
                *capture2.last_event_type.lock().unwrap(),
                Some(EventType::SyncStarted)
            );
            assert_eq!(
                *capture3.last_event_type.lock().unwrap(),
                Some(EventType::SyncStarted)
            );
        }

        replicant_destroy(engine);
    }
}

#[test]
#[cfg(debug_assertions)]
fn test_ffi_all_event_types() {
    unsafe {
        let engine = create_test_engine();
        assert!(!engine.is_null(), "Failed to create sync engine");

        let doc_capture = DocumentCapture::new();
        let sync_capture = SyncCapture::new();
        let error_capture = ErrorCapture::new();
        let conn_capture = ConnectionCapture::new();

        // Register for all event types
        replicant_register_document_callback(
            engine,
            document_capture_callback,
            &doc_capture as *const DocumentCapture as *mut c_void,
            -1,
        );
        replicant_register_sync_callback(
            engine,
            sync_capture_callback,
            &sync_capture as *const SyncCapture as *mut c_void,
        );
        replicant_register_error_callback(
            engine,
            error_capture_callback,
            &error_capture as *const ErrorCapture as *mut c_void,
        );
        replicant_register_connection_callback(
            engine,
            connection_capture_callback,
            &conn_capture as *const ConnectionCapture as *mut c_void,
        );

        // Test document events (0-2)
        for event_type in 0..3 {
            doc_capture.reset();
            replicant_emit_test_event(engine, event_type);
            replicant_process_events(engine, ptr::null_mut());
            assert_eq!(
                doc_capture.call_count.load(Ordering::SeqCst),
                1,
                "Document event type {} was not emitted",
                event_type
            );
        }

        // Test sync events (3-4)
        replicant_emit_test_event(engine, 3); // SyncStarted
        replicant_process_events(engine, ptr::null_mut());
        assert_eq!(sync_capture.call_count.load(Ordering::SeqCst), 1);

        replicant_emit_test_event(engine, 4); // SyncCompleted
        replicant_process_events(engine, ptr::null_mut());
        assert_eq!(sync_capture.call_count.load(Ordering::SeqCst), 2);

        // Test error event (5)
        replicant_emit_test_event(engine, 5); // SyncError
        replicant_process_events(engine, ptr::null_mut());
        assert_eq!(error_capture.call_count.load(Ordering::SeqCst), 1);

        // Test connection events (7-9)
        replicant_emit_test_event(engine, 7); // ConnectionLost
        replicant_process_events(engine, ptr::null_mut());
        assert_eq!(conn_capture.call_count.load(Ordering::SeqCst), 1);

        replicant_emit_test_event(engine, 8); // ConnectionAttempted
        replicant_process_events(engine, ptr::null_mut());
        assert_eq!(conn_capture.call_count.load(Ordering::SeqCst), 2);

        replicant_emit_test_event(engine, 9); // ConnectionSucceeded
        assert!(pump_events_until(engine, Duration::from_secs(1), || {
            conn_capture.call_count.load(Ordering::SeqCst) == 3
        }));

        replicant_destroy(engine);
    }
}

#[test]
#[cfg(debug_assertions)]
fn test_ffi_event_data_integrity() {
    unsafe {
        let engine = create_test_engine();
        assert!(!engine.is_null(), "Failed to create sync engine");

        let sync_capture = SyncCapture::new();
        let error_capture = ErrorCapture::new();
        let conn_capture = ConnectionCapture::new();

        replicant_register_sync_callback(
            engine,
            sync_capture_callback,
            &sync_capture as *const SyncCapture as *mut c_void,
        );
        replicant_register_error_callback(
            engine,
            error_capture_callback,
            &error_capture as *const ErrorCapture as *mut c_void,
        );
        replicant_register_connection_callback(
            engine,
            connection_capture_callback,
            &conn_capture as *const ConnectionCapture as *mut c_void,
        );

        // Test sync completed event (has document count)
        replicant_emit_test_event(engine, 4); // SyncCompleted
        replicant_process_events(engine, ptr::null_mut());

        assert_eq!(sync_capture.call_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            *sync_capture.last_event_type.lock().unwrap(),
            Some(EventType::SyncCompleted)
        );
        assert_eq!(*sync_capture.last_document_count.lock().unwrap(), 5); // Test uses fixed value of 5

        // Test sync error event (has error message)
        replicant_emit_test_event(engine, 5); // SyncError
        replicant_process_events(engine, ptr::null_mut());

        assert_eq!(error_capture.call_count.load(Ordering::SeqCst), 1);
        let error_msg = error_capture.last_error.lock().unwrap();
        assert!(error_msg.is_some());
        assert!(error_msg.as_ref().unwrap().contains("Test error message"));

        // Test connection event
        replicant_emit_test_event(engine, 7); // ConnectionLost
        replicant_process_events(engine, ptr::null_mut());

        assert_eq!(conn_capture.call_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            *conn_capture.last_event_type.lock().unwrap(),
            Some(EventType::ConnectionLost)
        );

        replicant_destroy(engine);
    }
}

#[test]
#[cfg(debug_assertions)]
fn test_ffi_event_burst() {
    unsafe {
        let engine = create_test_engine();
        assert!(!engine.is_null(), "Failed to create sync engine");

        let capture = DocumentCapture::new();

        replicant_register_document_callback(
            engine,
            document_capture_callback,
            &capture as *const DocumentCapture as *mut c_void,
            0, // Only DocumentCreated events
        );

        // Emit a burst of 5 events
        replicant_emit_test_event_burst(engine, 5);
        replicant_process_events(engine, ptr::null_mut());

        // Should have received all 5 events
        assert_eq!(capture.call_count.load(Ordering::SeqCst), 5);
        assert_eq!(
            *capture.last_event_type.lock().unwrap(),
            Some(EventType::DocumentCreated)
        );

        // The last event should have sequence 4 in the title
        let last_title = capture.last_title.lock().unwrap();
        assert!(last_title.is_some());
        assert!(last_title.as_ref().unwrap().contains("4")); // Sequence is 0-indexed

        replicant_destroy(engine);
    }
}

#[test]
fn test_ffi_null_pointer_safety() {
    unsafe {
        // Test that null pointers are handled safely

        // Test null engine for document callback
        let result = replicant_register_document_callback(
            ptr::null_mut(),
            document_capture_callback,
            ptr::null_mut(),
            -1,
        );
        assert_eq!(result, SyncResult::ErrorInvalidInput);

        // Test with valid engine but invalid filter
        let engine = create_test_engine();
        if !engine.is_null() {
            let result = replicant_register_document_callback(
                engine,
                document_capture_callback,
                ptr::null_mut(),
                999, // Invalid event type
            );
            assert_eq!(result, SyncResult::ErrorInvalidInput);

            replicant_destroy(engine);
        }
    }
}

#[test]
fn test_ffi_engine_lifecycle() {
    unsafe {
        // Test creating and destroying multiple engines
        for _ in 0..3 {
            let engine = create_test_engine();
            assert!(!engine.is_null(), "Failed to create sync engine");

            let capture = DocumentCapture::new();

            let result = replicant_register_document_callback(
                engine,
                document_capture_callback,
                &capture as *const DocumentCapture as *mut c_void,
                -1,
            );
            assert_eq!(result, SyncResult::Success);

            replicant_destroy(engine);
            // After destruction, the engine pointer should not be used
        }
    }
}

#[test]
fn test_ffi_callback_thread_safety() {
    use std::thread;
    use std::time::Duration;

    unsafe {
        let engine = create_test_engine();
        assert!(!engine.is_null(), "Failed to create sync engine");

        let capture = Arc::new(SyncCapture::new());
        let capture_clone = capture.clone();

        // Register callback
        replicant_register_sync_callback(
            engine,
            sync_capture_callback,
            Arc::as_ptr(&capture_clone) as *mut c_void,
        );

        #[cfg(debug_assertions)]
        {
            // Emit events from multiple threads (simulated stress test)
            let engine_ptr = engine as usize; // Store as usize for thread safety

            let handles: Vec<_> = (0..3)
                .map(|_i| {
                    let _capture = capture.clone();
                    thread::spawn(move || {
                        let engine = engine_ptr as *mut Replicant;
                        for _ in 0..2 {
                            replicant_emit_test_event(engine, 3); // SyncStarted
                            thread::sleep(Duration::from_millis(1));
                        }
                    })
                })
                .collect();

            // Wait for all threads
            for handle in handles {
                handle.join().unwrap();
            }

            // Process all queued events
            replicant_process_events(engine, ptr::null_mut());

            // Should have received 6 events total (3 threads × 2 events each)
            assert_eq!(capture.call_count.load(Ordering::SeqCst), 6);
        }

        replicant_destroy(engine);
    }
}

// =============================================================================
// Tests for Read APIs (get_document, get_all_documents, count_documents, etc.)
// =============================================================================

#[test]
fn test_ffi_get_document() {
    unsafe {
        let engine = create_test_engine();
        assert!(!engine.is_null(), "Failed to create sync engine");

        // Create a document first
        let content =
            CString::new(r#"{"title":"Test Get Document","data":"some content"}"#).unwrap();
        let mut doc_id_buf = [0u8; 37];

        let create_result =
            replicant_create_document(engine, content.as_ptr(), doc_id_buf.as_mut_ptr() as *mut i8);
        assert_eq!(create_result, SyncResult::Success);

        let doc_id = CStr::from_ptr(doc_id_buf.as_ptr() as *const i8)
            .to_string_lossy()
            .to_string();

        // Now retrieve the document
        let doc_id_cstr = CString::new(doc_id.clone()).unwrap();
        let mut out_content: *mut i8 = ptr::null_mut();

        let get_result = replicant_get_document(engine, doc_id_cstr.as_ptr(), &mut out_content);

        assert_eq!(get_result, SyncResult::Success);
        assert!(!out_content.is_null());

        let content_str = CStr::from_ptr(out_content).to_string_lossy().to_string();
        assert!(content_str.contains("Test Get Document"));
        assert!(content_str.contains("some content"));
        assert!(content_str.contains(&doc_id)); // Should include the document ID

        // Free the returned string
        replicant_string_free(out_content);

        replicant_destroy(engine);
    }
}

#[test]
fn test_ffi_get_document_not_found() {
    unsafe {
        let engine = create_test_engine();
        assert!(!engine.is_null(), "Failed to create sync engine");

        // Try to get a non-existent document
        let fake_id = CString::new("00000000-0000-0000-0000-000000000000").unwrap();
        let mut out_content: *mut i8 = ptr::null_mut();

        let result = replicant_get_document(engine, fake_id.as_ptr(), &mut out_content);

        assert_eq!(result, SyncResult::ErrorInvalidInput);

        replicant_destroy(engine);
    }
}

#[test]
fn test_ffi_get_all_documents_empty() {
    unsafe {
        let engine = create_test_engine();
        assert!(!engine.is_null(), "Failed to create sync engine");

        let mut out_documents: *mut i8 = ptr::null_mut();

        let result = replicant_get_all_documents(engine, &mut out_documents);

        assert_eq!(result, SyncResult::Success);
        assert!(!out_documents.is_null());

        let docs_str = CStr::from_ptr(out_documents).to_string_lossy().to_string();
        assert_eq!(docs_str, "[]"); // Empty array

        replicant_string_free(out_documents);
        replicant_destroy(engine);
    }
}

#[test]
fn test_ffi_get_all_documents_with_data() {
    unsafe {
        let engine = create_test_engine();
        assert!(!engine.is_null(), "Failed to create sync engine");

        // Create multiple documents
        let mut doc_ids: Vec<String> = Vec::new();
        for i in 0..3 {
            let content =
                CString::new(format!(r#"{{"title":"Document {}","index":{}}}"#, i, i)).unwrap();
            let mut doc_id_buf = [0u8; 37];

            let result = replicant_create_document(
                engine,
                content.as_ptr(),
                doc_id_buf.as_mut_ptr() as *mut i8,
            );
            assert_eq!(result, SyncResult::Success);

            let doc_id = CStr::from_ptr(doc_id_buf.as_ptr() as *const i8)
                .to_string_lossy()
                .to_string();
            doc_ids.push(doc_id);
        }

        // Get all documents
        let mut out_documents: *mut i8 = ptr::null_mut();
        let result = replicant_get_all_documents(engine, &mut out_documents);

        assert_eq!(result, SyncResult::Success);
        assert!(!out_documents.is_null());

        let docs_str = CStr::from_ptr(out_documents).to_string_lossy().to_string();

        // Verify all document IDs are present
        for doc_id in &doc_ids {
            assert!(
                docs_str.contains(doc_id),
                "Document {} not found in result",
                doc_id
            );
        }

        // Verify it's a JSON array with 3 documents
        assert!(docs_str.starts_with('['));
        assert!(docs_str.ends_with(']'));
        assert!(docs_str.contains("Document 0"));
        assert!(docs_str.contains("Document 1"));
        assert!(docs_str.contains("Document 2"));

        replicant_string_free(out_documents);
        replicant_destroy(engine);
    }
}

#[test]
fn test_ffi_count_documents() {
    unsafe {
        let engine = create_test_engine();
        assert!(!engine.is_null(), "Failed to create sync engine");

        // Initially should be 0
        let mut count: u64 = 999;
        let result = replicant_count_documents(engine, &mut count);
        assert_eq!(result, SyncResult::Success);
        assert_eq!(count, 0);

        // Create some documents
        for i in 0..5 {
            let content = CString::new(format!(r#"{{"title":"Doc {}"}}"#, i)).unwrap();
            let mut doc_id_buf = [0u8; 37];
            let create_result = replicant_create_document(
                engine,
                content.as_ptr(),
                doc_id_buf.as_mut_ptr() as *mut i8,
            );
            assert_eq!(create_result, SyncResult::Success);
        }

        // Count should now be 5
        let result = replicant_count_documents(engine, &mut count);
        assert_eq!(result, SyncResult::Success);
        assert_eq!(count, 5);

        replicant_destroy(engine);
    }
}

#[test]
fn test_ffi_is_connected() {
    unsafe {
        let engine = create_test_engine();
        assert!(!engine.is_null(), "Failed to create sync engine");

        // Should be disconnected (no server running)
        let connected = replicant_is_connected(engine);
        assert!(!connected);

        replicant_destroy(engine);
    }
}

#[test]
fn test_ffi_count_pending_sync() {
    unsafe {
        let engine = create_test_engine();
        assert!(!engine.is_null(), "Failed to create sync engine");

        // Initially should have no pending syncs
        let mut pending_count: u64 = 999;
        let result = replicant_count_pending_sync(engine, &mut pending_count);
        assert_eq!(result, SyncResult::Success);
        assert_eq!(pending_count, 0);

        // Create a document (should be pending sync since we're offline)
        let content = CString::new(r#"{"title":"Pending Doc"}"#).unwrap();
        let mut doc_id_buf = [0u8; 37];
        let create_result =
            replicant_create_document(engine, content.as_ptr(), doc_id_buf.as_mut_ptr() as *mut i8);
        assert_eq!(create_result, SyncResult::Success);

        // Should now have 1 pending sync
        let result = replicant_count_pending_sync(engine, &mut pending_count);
        assert_eq!(result, SyncResult::Success);
        assert_eq!(pending_count, 1);

        replicant_destroy(engine);
    }
}

#[test]
fn test_ffi_read_api_null_safety() {
    unsafe {
        // Test null engine pointer for all read APIs
        let mut out_content: *mut i8 = ptr::null_mut();
        let mut out_count: u64 = 0;
        let fake_id = CString::new("00000000-0000-0000-0000-000000000000").unwrap();

        // get_document with null engine
        let result = replicant_get_document(ptr::null_mut(), fake_id.as_ptr(), &mut out_content);
        assert_eq!(result, SyncResult::ErrorInvalidInput);

        // get_all_documents with null engine
        let result = replicant_get_all_documents(ptr::null_mut(), &mut out_content);
        assert_eq!(result, SyncResult::ErrorInvalidInput);

        // count_documents with null engine
        let result = replicant_count_documents(ptr::null_mut(), &mut out_count);
        assert_eq!(result, SyncResult::ErrorInvalidInput);

        // count_pending_sync with null engine
        let result = replicant_count_pending_sync(ptr::null_mut(), &mut out_count);
        assert_eq!(result, SyncResult::ErrorInvalidInput);

        // is_connected with null engine should return false
        let connected = replicant_is_connected(ptr::null_mut());
        assert!(!connected);
    }
}

#[test]
fn test_ffi_update_and_get_document() {
    unsafe {
        let engine = create_test_engine();
        assert!(!engine.is_null(), "Failed to create sync engine");

        // Create a document
        let content = CString::new(r#"{"title":"Original Title","value":1}"#).unwrap();
        let mut doc_id_buf = [0u8; 37];

        let create_result =
            replicant_create_document(engine, content.as_ptr(), doc_id_buf.as_mut_ptr() as *mut i8);
        assert_eq!(create_result, SyncResult::Success);

        let doc_id = CStr::from_ptr(doc_id_buf.as_ptr() as *const i8)
            .to_string_lossy()
            .to_string();
        let doc_id_cstr = CString::new(doc_id.clone()).unwrap();

        // Update the document
        let updated_content = CString::new(r#"{"title":"Updated Title","value":42}"#).unwrap();
        let update_result =
            replicant_update_document(engine, doc_id_cstr.as_ptr(), updated_content.as_ptr());
        assert_eq!(update_result, SyncResult::Success);

        // Retrieve and verify the content field has updated values
        let mut out_content: *mut i8 = ptr::null_mut();
        let get_result = replicant_get_document(engine, doc_id_cstr.as_ptr(), &mut out_content);
        assert_eq!(get_result, SyncResult::Success);

        let content_str = CStr::from_ptr(out_content).to_string_lossy().to_string();
        let doc: serde_json::Value = serde_json::from_str(&content_str).unwrap();

        let doc_content = &doc["content"];
        assert_eq!(doc_content["title"], "Updated Title");
        assert_eq!(doc_content["value"], 42);

        replicant_string_free(out_content);
        replicant_destroy(engine);
    }
}

#[test]
fn test_ffi_offline_startup_reports_no_connection_or_sync() {
    unsafe {
        // Nothing listens on port 1, so every connect is refused at once.
        let db_path = unique_db_path();
        let user_id = uuid::Uuid::new_v4().to_string();
        let engine = create_test_engine_with(
            &db_path,
            "ws://127.0.0.1:1/socket/websocket",
            Some(&user_id),
        );
        assert!(!engine.is_null(), "Failed to create sync engine");

        let log = EventLog::default();
        register_event_log(engine, &log);

        // The start-up connect fails, then the reconnect monitor's first try does.
        let retried = pump_events_until(engine, Duration::from_secs(10), || {
            log.count(EventType::SyncError) >= 2
        });
        assert!(retried, "both connects should fail: {:?}", log.events());
        pump_events_until(engine, Duration::from_millis(200), || false);

        assert_eq!(
            log.position(EventType::ConnectionSucceeded),
            None,
            "{:?}",
            log.events()
        );
        assert_eq!(
            log.position(EventType::SyncCompleted),
            None,
            "{:?}",
            log.events()
        );
        assert!(!replicant_is_connected(engine));

        destroy_and_remove_db(engine, &db_path);
    }
}

#[test]
fn test_ffi_local_only_startup_reports_no_connection_or_sync() {
    unsafe {
        let db_path = unique_db_path();
        let engine = create_test_engine_with(&db_path, "ws://localhost:8080/ws", None);
        assert!(!engine.is_null(), "Failed to create sync engine");

        let log = EventLog::default();
        register_event_log(engine, &log);

        // A local-only start has no event marking its end, so watch a short window.
        pump_events_until(engine, Duration::from_millis(500), || false);

        assert_eq!(
            log.count(EventType::ConnectionSucceeded),
            0,
            "{:?}",
            log.events()
        );
        assert_eq!(log.count(EventType::SyncCompleted), 0, "{:?}", log.events());

        destroy_and_remove_db(engine, &db_path);
    }
}
