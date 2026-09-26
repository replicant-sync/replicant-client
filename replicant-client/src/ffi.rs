//! C FFI interface for the sync client
//!
//! This module provides C-compatible functions for using the sync client from C/C++.
//! The generated header file will be available after building.

use serde_json::Value;
use std::cell::Cell;
use std::ffi::{c_void, CStr, CString};
use std::os::raw::c_char;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use uuid::Uuid;

use crate::error_code::ReplicantErrorCode;
use crate::events::{
    ConflictEventCallback, ConnectionEventCallback, DocumentEventCallback, ErrorEventCallback,
    EventDispatcher, EventOrigin, EventType, IdentityEventCallback, SyncEventCallback,
};
use crate::{Client as CoreClient, ClientDatabase};

/// Opaque handle to a Replicant client instance
pub struct Replicant {
    engine: Arc<std::sync::Mutex<Option<Arc<CoreClient>>>>,
    database: Arc<ClientDatabase>,
    runtime: Runtime,
    pub(crate) event_dispatcher: Arc<EventDispatcher>,
    /// The background task `replicant_create` spawns to build the sync engine.
    /// Kept so shutdown can wait for it instead of cancelling it: cancelling it
    /// mid-`establish()` is what orphaned a sqlite worker thread in DEV-1118.
    init_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Identifies this instance's runtime threads, so shutdown can tell that it
    /// was called from inside its own runtime (where joining would deadlock).
    instance_id: u64,
}

/// `begin_teardown` hands the handle to its thread as an address, which erases
/// the `Send` bound the compiler would otherwise check. Keep checking it here,
/// so that adding a `!Send` field to `Replicant` fails to build instead of
/// silently moving something across threads.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Replicant>();
};

static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// `instance_id` of the Replicant whose tokio runtime owns this thread, or
    /// 0 for any other thread.
    static RUNTIME_OWNER: Cell<u64> = const { Cell::new(0) };
}

/// A teardown started by `replicant_destroy`, and whether the caller is allowed
/// to wait for it.
enum Teardown {
    /// Running on its own thread, which this caller may join.
    Waitable(std::thread::JoinHandle<()>),
    /// Running, but not joinable from this caller — or never started at all.
    /// Either way nothing may block on it.
    Detached,
}

impl Replicant {
    /// Closes everything this handle owns and joins every thread it started,
    /// then frees it. Always runs on the dedicated thread `begin_teardown`
    /// spawns, so that dropping the runtime is never done from inside that
    /// runtime, and so that no FFI caller is held up by it.
    ///
    /// Deliberately unbounded. Nothing waits on this thread unless a caller
    /// asks to, so there is no host to hang — and every bound one could impose
    /// here would mean *abandoning* a pool mid-close, which is the very thing
    /// this exists to prevent. It finishes when sqlite lets it finish (each step
    /// is bounded in practice by sqlite's 5s busy timeout per contended
    /// statement).
    ///
    /// Order matters, and every pool close is unconditional — nothing that could
    /// stall may stand between this function and a `Pool::close()`, because
    /// `Pool::close()` is the only thing that joins a sqlite worker thread
    /// (dropping the runtime joins tokio's threads and no others):
    ///
    /// 1. Close the handle's own pool, awaited. First, so that a hang anywhere
    ///    later cannot keep it open. It is independent of the sync engine's pool,
    ///    so closing it early disturbs nothing. `Pool::close()` resolves only
    ///    when the pool's size reaches zero, i.e. every connection — including
    ///    ones still being established — is done.
    /// 2. Wait for the background init task. Its `ClientDatabase::new` may have
    ///    a sqlx worker thread sitting inside `establish()` retrying on
    ///    SQLITE_BUSY; cancelling the task (which is what dropping the runtime
    ///    does) abandons that thread *and* the pool handle that could have
    ///    closed it. Awaiting the task means that pool always exists to close.
    ///    Bounded in practice by the socket connect timeout and sqlite's busy
    ///    timeout, both of which are finite.
    /// 3. Close the sync engine's pool (and release its socket best-effort — see
    ///    `Client::shutdown`, which closes the pool before touching the socket
    ///    for the same reason).
    /// 4. Drop the runtime. A plain drop joins the runtime's worker and
    ///    blocking threads (`shutdown_background`/`shutdown_timeout` would
    ///    deliberately not), so on return no tokio thread of ours is left.
    fn shutdown(self) {
        let Replicant {
            engine,
            database,
            runtime,
            event_dispatcher,
            init_task,
            instance_id,
        } = self;

        let started = Instant::now();

        // Poisoning must never be read as "nothing to close". Every other FFI
        // function holds these guards across a `block_on` with `unwrap()`, so a
        // single panicking operation poisons the mutex — and skipping the close
        // would leave behind exactly the threads this exists to join.
        let init_task = init_task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();

        // 1. The handle's own pool, before anything that could stall.
        runtime.block_on(database.close());
        drop(database);

        // 2. Background init: let it finish so nothing is abandoned mid-connect.
        if let Some(handle) = init_task {
            if let Err(e) = runtime.block_on(handle) {
                tracing::warn!("FFI: background init ended abnormally during shutdown: {e}");
            }
        }

        // 3. The sync engine's own pool, then its socket.
        let client = engine
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(client) = client {
            runtime.block_on(client.shutdown());
            drop(client);
        }
        drop(engine);
        drop(event_dispatcher);

        // 4. Joins the runtime's worker and blocking threads.
        drop(runtime);

        tracing::info!(
            "FFI: Replicant {} shut down in {:?} ({} sqlite worker thread(s) started \
             by this process)",
            instance_id,
            started.elapsed(),
            crate::database::sqlite_workers_started()
        );
    }

    /// Runs `fut` on this instance's runtime, marking the calling thread as one
    /// of that runtime's own for the duration.
    ///
    /// Every FFI function that drives async work goes through here. The mark is
    /// what makes a *re-entrant* destroy — one issued from a callback dispatched
    /// while an FFI call is still on this thread's stack — take the `Detached`
    /// path instead of waiting for a teardown that cannot finish until this
    /// frame returns. (Freeing the handle from inside a call that is still using
    /// it is a use-after-free no library can undo; the mark keeps it from also
    /// being a deadlock.)
    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        struct Restore(u64);
        impl Drop for Restore {
            fn drop(&mut self) {
                RUNTIME_OWNER.with(|owner| owner.set(self.0));
            }
        }

        let _restore = Restore(RUNTIME_OWNER.with(|owner| owner.replace(self.instance_id)));
        self.runtime.block_on(fut)
    }

    /// Hands the handle to a dedicated thread that tears it down, and reports
    /// whether this caller may wait for that thread.
    ///
    /// The teardown never runs on the caller's thread: dropping a tokio runtime
    /// from inside itself panics, and joining a runtime's threads from one of
    /// them deadlocks. A caller that *is* one of this instance's runtime threads
    /// (a destroy from inside a Replicant callback, say) therefore gets
    /// `Detached` — the teardown still happens, it just cannot be waited for
    /// from there — rather than a deadlock or a panic across the FFI boundary.
    fn begin_teardown(handle: Box<Replicant>) -> Teardown {
        let instance_id = handle.instance_id;
        let called_from_own_runtime = RUNTIME_OWNER.with(|owner| owner.get()) == instance_id;

        // Handed over as an address, not as a `Box`, so that a failed spawn
        // leaks the handle instead of dropping it on the caller's thread. That
        // drop would block the caller joining the runtime — against this
        // function's whole contract — and would panic outright if the caller is
        // itself a runtime thread. A leak is the lesser evil: the memory stays
        // valid for whatever is still running in it.
        let raw = Box::into_raw(handle) as usize;

        let worker = std::thread::Builder::new()
            .name("replicant-shutdown".to_string())
            .spawn(move || {
                // SAFETY: `raw` came from `Box::into_raw` just above and is
                // handed to exactly one thread, which is the only owner now.
                unsafe { Box::from_raw(raw as *mut Replicant) }.shutdown()
            });

        match worker {
            Err(e) => {
                tracing::error!(
                    "FFI: could not spawn the shutdown thread ({e}); \
                     Replicant {instance_id} was leaked deliberately and its \
                     threads will outlive this handle"
                );
                Teardown::Detached
            }
            Ok(_) if called_from_own_runtime => {
                tracing::warn!(
                    "FFI: Replicant {} was destroyed from its own runtime thread; \
                     its teardown cannot be waited for from there.",
                    instance_id
                );
                Teardown::Detached
            }
            Ok(worker) => Teardown::Waitable(worker),
        }
    }
}

/// Waits up to `timeout` for a teardown thread, without ever joining one that
/// cannot be joined from here. Returns whether it finished.
fn wait_for_teardown(teardown: Teardown, timeout: Duration) -> bool {
    let worker = match teardown {
        Teardown::Waitable(worker) => worker,
        Teardown::Detached => return false,
    };

    let deadline = Instant::now() + timeout;
    while !worker.is_finished() {
        if Instant::now() >= deadline {
            // The thread keeps running: abandoning the teardown is what leaves
            // threads in an unloadable module, so it is never cut short.
            tracing::warn!(
                "FFI: teardown did not finish within {:?} and is still running \
                 ({} sqlite worker thread(s) started by this process)",
                timeout,
                crate::database::sqlite_workers_started()
            );
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    // A panic in the teardown thread means the teardown did NOT complete: say so
    // rather than reporting a clean shutdown.
    match worker.join() {
        Ok(()) => true,
        Err(_) => {
            tracing::error!(
                "FFI: the teardown thread panicked; pools may not have been closed and \
                 Replicant threads may still be running"
            );
            false
        }
    }
}

/// Result codes for C API functions
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SyncResult {
    Success = 0,
    ErrorInvalidInput = -1,
    ErrorConnection = -2,
    ErrorDatabase = -3,
    ErrorSerialization = -4,
    ErrorUnknown = -99,
}

/// Document structure for C API
#[repr(C)]
pub struct Document {
    pub id: *mut c_char,
    pub title: *mut c_char,
    pub content: *mut c_char,
    pub sync_revision: i64,
}

/// Create a new sync engine instance
///
/// # Arguments
/// * `database_url` - SQLite database URL (e.g., "sqlite:client.db?mode=rwc")
/// * `server_url` - WebSocket server URL (e.g., "ws://localhost:8080/ws")
/// * `email` - User email address
/// * `api_key` - Application API key (rpa_ prefix)
/// * `api_secret` - Application API secret (rps_ prefix)
///
/// # Returns
/// * Pointer to SyncEngine on success, null on failure
///
/// # Events
/// Connection and initial sync run in the background. ConnectionSucceeded is
/// emitted once per successful connect (at start-up or on a later reconnect),
/// after the engine uses that connection. SyncStarted and SyncCompleted then
/// bracket the full sync that follows; SyncCompleted means the server answered.
/// An offline start emits neither until the server is reached. An engine with
/// no API key, or whose database has never adopted an identity (no `user_id`
/// on this or an earlier run), never connects, so it emits neither.
///
/// # Safety
/// Caller must ensure all pointers are valid, non-null C strings
#[no_mangle]
pub unsafe extern "C" fn replicant_create(
    database_url: *const c_char,
    server_url: *const c_char,
    email: *const c_char,
    api_key: *const c_char,
    api_secret: *const c_char,
    user_id: *const c_char,
) -> *mut Replicant {
    if database_url.is_null()
        || server_url.is_null()
        || email.is_null()
        || api_key.is_null()
        || api_secret.is_null()
    {
        return ptr::null_mut();
    }

    // Canonical user id from stored credentials; null means no enrolled
    // identity (offline/local-only). A non-null id must be a real UUID.
    let canonical_user_id = if user_id.is_null() {
        None
    } else {
        match CStr::from_ptr(user_id)
            .to_str()
            .ok()
            .and_then(|s| Uuid::parse_str(s).ok())
        {
            Some(id) if !id.is_nil() => Some(id),
            _ => return ptr::null_mut(),
        }
    };

    let database_url = match CStr::from_ptr(database_url).to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    let server_url = match CStr::from_ptr(server_url).to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    let email = match CStr::from_ptr(email).to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    let api_key = match CStr::from_ptr(api_key).to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    let api_secret = match CStr::from_ptr(api_secret).to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    let instance_id = NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed);

    // Equivalent to `Runtime::new()`, plus named threads and an owner marker so
    // shutdown can recognise its own runtime threads (see `destroy_blocking`).
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name(format!("replicant-{instance_id}"))
        .on_thread_start(move || RUNTIME_OWNER.with(|owner| owner.set(instance_id)))
        .on_thread_stop(|| RUNTIME_OWNER.with(|owner| owner.set(0)))
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return ptr::null_mut(),
    };

    let database = match runtime.block_on(async { ClientDatabase::new(database_url).await }) {
        Ok(db) => Arc::new(db),
        Err(_) => return ptr::null_mut(),
    };

    // Failing here hands no handle back, so nothing will ever call
    // `replicant_destroy` for this pool: close it on the way out, or its sqlite
    // worker threads outlive a `replicant_create` the caller believes did
    // nothing. Contended creation (migrations blocked on SQLITE_BUSY) is
    // exactly when this path is taken.
    let close_and_fail = |runtime: Runtime, database: Arc<ClientDatabase>| -> *mut Replicant {
        runtime.block_on(database.close());
        drop(database);
        drop(runtime);
        ptr::null_mut()
    };

    // Run migrations
    if runtime
        .block_on(async { database.run_migrations().await })
        .is_err()
    {
        return close_and_fail(runtime, database);
    }

    // Ensure user config exists so offline operations work immediately
    if runtime
        .block_on(async {
            database
                .ensure_user_config_with_identifier(server_url, email)
                .await
        })
        .is_err()
    {
        return close_and_fail(runtime, database);
    }

    let event_dispatcher = Arc::new(EventDispatcher::new());
    let engine = Arc::new(std::sync::Mutex::new(None));

    // Spawn background task to create sync engine (connect + initial sync)
    let engine_slot = engine.clone();
    let event_dispatcher_clone = event_dispatcher.clone();
    let database_url = database_url.to_string();
    let server_url = server_url.to_string();
    let email = email.to_string();
    let api_key = api_key.to_string();
    let api_secret = api_secret.to_string();
    let init_task = runtime.spawn(async move {
        match CoreClient::open(
            &database_url,
            &server_url,
            &email,
            &api_key,
            &api_secret,
            canonical_user_id,
            Some(event_dispatcher_clone.clone()),
        )
        .await
        {
            Ok(client) => {
                let client = Arc::new(client);
                // Never panic here: the client is already built, and unwinding
                // out of this task would drop it — and its pool — un-closed,
                // leaving a sqlx worker thread per connection running in this
                // module. A poisoned mutex is recovered instead, so shutdown
                // still finds the client and can close it.
                // The slot must be empty: this task is its only writer, and
                // teardown waits for this task before taking the slot. If that
                // ever stopped holding, the assignment would drop a live client
                // — and its pool — un-closed, which is the hazard itself.
                match engine_slot.lock() {
                    Ok(mut slot) => {
                        debug_assert!(slot.is_none(), "engine slot was already occupied");
                        *slot = Some(client.clone());
                    }
                    Err(poisoned) => {
                        tracing::warn!(
                            "FFI: engine slot mutex was poisoned; storing the client anyway \
                             so its pool can still be closed"
                        );
                        let mut slot = poisoned.into_inner();
                        debug_assert!(slot.is_none(), "engine slot was already occupied");
                        *slot = Some(client.clone());
                    }
                }
                // Started only once it is in the slot, so a consumer reacting to
                // ConnectionSucceeded or SyncCompleted already uses this client,
                // and documents written through the offline path meanwhile are
                // uploaded by its initial sync.
                if let Err(e) = client.start().await {
                    event_dispatcher_clone.emit_sync_error(
                        ReplicantErrorCode::Unknown,
                        &format!("Initial sync failed: {}", e),
                    );
                }
            }
            Err(e) => {
                // The pool this init opened is already closed:
                // `open` closes it on every failure path, which
                // is what joins its sqlite worker threads. Nothing is left to
                // clean up here, and the slot stays `None`.
                event_dispatcher_clone.emit_sync_error(
                    ReplicantErrorCode::Unknown,
                    &format!("Background init failed: {}", e),
                );
            }
        }
    });

    Box::into_raw(Box::new(Replicant {
        engine,
        database,
        runtime,
        event_dispatcher,
        init_task: std::sync::Mutex::new(Some(init_task)),
        instance_id,
    }))
}

/// Destroy a sync engine instance and free memory
///
/// NON-BLOCKING, fire-and-forget: it returns immediately and the teardown —
/// closing both sqlite pools and joining this instance's threads — finishes on
/// a Replicant-owned thread afterwards. The handle is invalid the moment this
/// returns; no further call may use it.
///
/// This is safe to call on a UI or audio-host thread (plugin scans, project
/// close) precisely because it does not wait. The consequence is that threads
/// Replicant started can still be running Replicant code for a short time after
/// it returns — normally milliseconds, longer if sqlite is contended.
///
/// THEREFORE: any caller that can be UNLOADED FROM MEMORY — every plugin —
/// MUST make sure its own binary is never unmapped, or one of those threads
/// will be executing code at an address that no longer exists (the DEV-1118
/// crash: an execute access violation on an unmapped image). Pin the module
/// once, at load:
///
///   * Windows: `GetModuleHandleExW` with `GET_MODULE_HANDLE_EX_FLAG_PIN`
///     (plus `GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS`) on an address in your
///     own module.
///   * macOS / Linux: `dlopen` your own image with `RTLD_NOLOAD | RTLD_NODELETE`.
///
/// Pinning is the primary defence, not a workaround. A host process that only
/// ever exits (a standalone app) does not need it; anything a host can unload
/// does.
///
/// UPGRADING FROM 0.6.2 OR EARLIER WITHOUT PINNING IS A REGRESSION. The old
/// destroy dropped the handle, which joined the tokio runtime's threads before
/// returning and left only sqlx's workers running; this one returns with the
/// background init, both pools, the runtime's threads and the sqlx workers all
/// still live. The total time any thread of ours is alive goes down — the pools
/// are now actually closed, so nothing lingers for the life of the process — but
/// the exposure *at the moment destroy returns* goes up.
///
/// A second instance may be created immediately; it shares nothing with the one
/// being torn down. Note only that the outgoing instance may still be writing to
/// the same database file for a moment, so an immediate re-create against it can
/// see sqlite contention (and `replicant_create` returns NULL if that outlasts
/// sqlite's 5s busy timeout). `replicant_destroy_and_wait` avoids the overlap.
///
/// Callers that *can* afford to wait — standalone apps shutting down, tests —
/// should use `replicant_destroy_and_wait` instead, which reports whether the
/// teardown actually finished.
///
/// # Safety
/// Caller must ensure engine pointer was created by replicant_create and hasn't been freed.
/// As with any free function, no other call on this handle may be in progress on
/// any thread — which includes destroying it from inside a Replicant callback
/// while the call that dispatched that callback is still on the stack.
#[no_mangle]
pub unsafe extern "C" fn replicant_destroy(engine: *mut Replicant) {
    if engine.is_null() {
        return;
    }

    let handle = Box::from_raw(engine);

    // Nothing may unwind into C. A panic here (a poisoned lock, a failing
    // thread spawn) would otherwise cross the FFI boundary, which is UB.
    // Dropping the returned Teardown detaches the thread; it does not stop it.
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _ = Replicant::begin_teardown(handle);
    }))
    .is_err()
    {
        tracing::error!("FFI: replicant_destroy panicked; the handle was leaked deliberately");
    }
}

/// Destroy a sync engine instance and wait for its teardown to finish.
///
/// Same as `replicant_destroy` (the handle is invalid either way, whatever this
/// returns) but BLOCKS for up to `timeout_ms` waiting for Replicant to close
/// its sqlite pools and join its threads.
///
/// Returns `true` when the teardown completed: no REPLICANT-OWNED thread — its
/// tokio runtime, its sqlite workers — is running any more. That is the claim,
/// and it is narrower than "this module has no threads in it": libraries linked
/// into Replicant (the WebSocket and TLS stacks in particular) may keep threads
/// of their own that Replicant neither owns nor can join, so a module that has
/// ever connected should still be pinned rather than unloaded.
///
/// Returns `false` if the wait ran out, or if the caller cannot wait (destroy
/// from inside a Replicant callback on a Replicant runtime thread) — the teardown
/// then carries on in the background.
///
/// `timeout_ms` of 0 does not wait at all; it reports whether the teardown had
/// already finished. Pass a large value (`UINT32_MAX`) to wait in effect
/// indefinitely.
///
/// Intended for standalone applications and tests. A plugin should call
/// `replicant_destroy` and pin its module instead of blocking a host thread.
///
/// # Safety
/// Caller must ensure engine pointer was created by replicant_create and hasn't been freed
#[no_mangle]
pub unsafe extern "C" fn replicant_destroy_and_wait(
    engine: *mut Replicant,
    timeout_ms: u32,
) -> bool {
    if engine.is_null() {
        return true;
    }

    let handle = Box::from_raw(engine);

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let teardown = Replicant::begin_teardown(handle);
        wait_for_teardown(teardown, Duration::from_millis(u64::from(timeout_ms)))
    })) {
        Ok(finished) => finished,
        Err(_) => {
            tracing::error!(
                "FFI: replicant_destroy_and_wait panicked; the handle was leaked deliberately"
            );
            false
        }
    }
}

/// Create a new document
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `content_json` - Document content as JSON string (should include any title as part of the JSON)
/// * `out_document_id` - Output buffer for document ID (must be at least 37 chars)
///
/// # Returns
/// * CSyncResult indicating success or failure
///
/// # Safety
/// Caller must ensure engine is valid, content_json is a valid C string, and out_document_id has space for 37 bytes
#[no_mangle]
pub unsafe extern "C" fn replicant_create_document(
    engine: *mut Replicant,
    content_json: *const c_char,
    out_document_id: *mut c_char,
) -> SyncResult {
    if engine.is_null() || content_json.is_null() || out_document_id.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &mut *engine;

    let content_json = match CStr::from_ptr(content_json).to_str() {
        Ok(s) => s,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    let content: Value = match serde_json::from_str(content_json) {
        Ok(c) => c,
        Err(_) => return SyncResult::ErrorSerialization,
    };

    let engine_guard = engine.engine.lock().unwrap();
    let doc_id = if let Some(ref sync_engine) = *engine_guard {
        // Online mode - use sync engine
        match engine.block_on(async { sync_engine.create_document(content.clone()).await }) {
            Ok(doc) => {
                // Emit event to FFI event dispatcher
                engine
                    .event_dispatcher
                    .emit_document_created_with_attribution(
                        &doc.id,
                        &content,
                        doc.user_id.as_ref(),
                        doc.author_name.as_deref(),
                        doc.visibility.as_deref(),
                        EventOrigin::Local,
                    );
                doc.id
            }
            Err(_) => return SyncResult::ErrorConnection,
        }
    } else {
        drop(engine_guard);
        // Offline mode - create locally
        let doc_id = Uuid::new_v4();
        let user_id = match engine.block_on(async { engine.database.get_user_id().await }) {
            Ok(id) => id,
            Err(_) => return SyncResult::ErrorDatabase,
        };

        let doc = replicant_core::models::Document {
            id: doc_id,
            user_id: Some(user_id),
            content: content.clone(),
            sync_revision: 1,
            content_hash: None,
            title: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            deleted_at: None,
            author_name: None,
            visibility: None,
            provenance: None,
        };

        if engine
            .block_on(async { engine.database.save_document(&doc).await })
            .is_err()
        {
            return SyncResult::ErrorDatabase;
        }

        // Emit event for offline document creation
        engine
            .event_dispatcher
            .emit_document_created_with_attribution(
                &doc_id,
                &content,
                doc.user_id.as_ref(),
                doc.author_name.as_deref(),
                doc.visibility.as_deref(),
                EventOrigin::Local,
            );

        doc_id
    };

    // Copy document ID to output buffer
    let id_string = doc_id.to_string();
    let id_bytes = id_string.as_bytes();
    if id_bytes.len() >= 36 {
        unsafe {
            ptr::copy_nonoverlapping(id_bytes.as_ptr(), out_document_id as *mut u8, 36);
            out_document_id.add(36).write(0); // null terminator
        }
    }

    SyncResult::Success
}

/// Create a new document with a specified ID
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `document_id` - UUID string to use as the document ID
/// * `content_json` - Document content as JSON string
///
/// # Returns
/// * `SyncResult::Success` - Document created successfully
/// * `SyncResult::ErrorInvalidInput` - Invalid UUID format or null pointers
/// * `SyncResult::ErrorSerialization` - Invalid JSON content
/// * `SyncResult::ErrorDatabase` - Database operation failed
/// * `SyncResult::ErrorConnection` - Sync to server failed (document saved locally)
///
/// # Note
/// If a document with the specified ID already exists, it will be overwritten (upsert behavior).
/// Use this for ID preservation during data migration or import scenarios.
///
/// # Safety
/// Caller must ensure engine is valid, document_id and content_json are valid C strings
#[no_mangle]
pub unsafe extern "C" fn replicant_create_document_with_id(
    engine: *mut Replicant,
    document_id: *const c_char,
    content_json: *const c_char,
) -> SyncResult {
    if engine.is_null() || document_id.is_null() || content_json.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &mut *engine;

    let document_id_str = match CStr::from_ptr(document_id).to_str() {
        Ok(s) => s,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    let doc_id = match Uuid::parse_str(document_id_str) {
        Ok(id) => id,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    let content_json = match CStr::from_ptr(content_json).to_str() {
        Ok(s) => s,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    let content: Value = match serde_json::from_str(content_json) {
        Ok(c) => c,
        Err(_) => return SyncResult::ErrorSerialization,
    };

    let engine_guard = engine.engine.lock().unwrap();
    if let Some(ref sync_engine) = *engine_guard {
        // Online mode - use sync engine
        match engine.block_on(async {
            sync_engine
                .create_document_with_id(doc_id, content.clone())
                .await
        }) {
            Ok(doc) => {
                engine
                    .event_dispatcher
                    .emit_document_created_with_attribution(
                        &doc.id,
                        &content,
                        doc.user_id.as_ref(),
                        doc.author_name.as_deref(),
                        doc.visibility.as_deref(),
                        EventOrigin::Local,
                    );
            }
            Err(_) => return SyncResult::ErrorConnection,
        }
    } else {
        drop(engine_guard);
        // Offline mode - create locally
        let user_id = match engine.block_on(async { engine.database.get_user_id().await }) {
            Ok(id) => id,
            Err(_) => return SyncResult::ErrorDatabase,
        };

        let doc = replicant_core::models::Document {
            id: doc_id,
            user_id: Some(user_id),
            content: content.clone(),
            sync_revision: 1,
            content_hash: None,
            title: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            deleted_at: None,
            author_name: None,
            visibility: None,
            provenance: None,
        };

        if engine
            .block_on(async { engine.database.save_document(&doc).await })
            .is_err()
        {
            return SyncResult::ErrorDatabase;
        }

        engine
            .event_dispatcher
            .emit_document_created_with_attribution(
                &doc_id,
                &content,
                doc.user_id.as_ref(),
                doc.author_name.as_deref(),
                doc.visibility.as_deref(),
                EventOrigin::Local,
            );
    }

    SyncResult::Success
}

/// Update an existing document
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `document_id` - Document ID to update
/// * `content_json` - New document content as JSON string
///
/// # Returns
/// * CSyncResult indicating success or failure
///
/// # Safety
/// Caller must ensure engine is valid and both document_id and content_json are valid C strings
#[no_mangle]
pub unsafe extern "C" fn replicant_update_document(
    engine: *mut Replicant,
    document_id: *const c_char,
    content_json: *const c_char,
) -> SyncResult {
    if engine.is_null() || document_id.is_null() || content_json.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &mut *engine;

    let document_id = match CStr::from_ptr(document_id).to_str() {
        Ok(s) => s,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    let doc_uuid = match Uuid::parse_str(document_id) {
        Ok(id) => id,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    let content_json = match CStr::from_ptr(content_json).to_str() {
        Ok(s) => s,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    let content: Value = match serde_json::from_str(content_json) {
        Ok(c) => c,
        Err(_) => return SyncResult::ErrorSerialization,
    };

    let engine_guard = engine.engine.lock().unwrap();
    if let Some(ref sync_engine) = *engine_guard {
        // Online mode
        match engine.block_on(async { sync_engine.update_document(doc_uuid, content).await }) {
            Ok(_) => SyncResult::Success,
            Err(_) => SyncResult::ErrorConnection,
        }
    } else {
        drop(engine_guard);
        // Offline mode - update locally
        let doc = match engine.block_on(async { engine.database.get_document(&doc_uuid).await }) {
            Ok(d) => d,
            Err(_) => return SyncResult::ErrorDatabase,
        };

        let mut updated_doc = doc;
        updated_doc.content = content;
        updated_doc.sync_revision += 1;
        updated_doc.content_hash = None; // Will be recalculated on server
        updated_doc.updated_at = chrono::Utc::now();

        match engine.block_on(async { engine.database.save_document(&updated_doc).await }) {
            Ok(_) => {
                // Emit event for offline document update
                engine
                    .event_dispatcher
                    .emit_document_updated_with_attribution(
                        &doc_uuid,
                        &updated_doc.content,
                        updated_doc.user_id.as_ref(),
                        updated_doc.author_name.as_deref(),
                        updated_doc.visibility.as_deref(),
                        EventOrigin::Local,
                    );
                SyncResult::Success
            }
            Err(_) => SyncResult::ErrorDatabase,
        }
    }
}

/// Delete a document
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `document_id` - Document ID to delete
///
/// # Returns
/// * CSyncResult indicating success or failure
///
/// # Safety
/// Caller must ensure engine is valid and document_id is a valid C string
#[no_mangle]
pub unsafe extern "C" fn replicant_delete_document(
    engine: *mut Replicant,
    document_id: *const c_char,
) -> SyncResult {
    if engine.is_null() || document_id.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &mut *engine;

    let document_id = match CStr::from_ptr(document_id).to_str() {
        Ok(s) => s,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    let doc_uuid = match Uuid::parse_str(document_id) {
        Ok(id) => id,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    let engine_guard = engine.engine.lock().unwrap();
    if let Some(ref sync_engine) = *engine_guard {
        // Online mode
        match engine.block_on(async { sync_engine.delete_document(doc_uuid).await }) {
            Ok(_) => SyncResult::Success,
            Err(_) => SyncResult::ErrorConnection,
        }
    } else {
        drop(engine_guard);
        // Offline mode
        match engine.block_on(async { engine.database.delete_document(&doc_uuid).await }) {
            Ok(_) => {
                // Emit event for offline document deletion
                engine
                    .event_dispatcher
                    .emit_document_deleted(&doc_uuid, EventOrigin::Local);
                SyncResult::Success
            }
            Err(_) => SyncResult::ErrorDatabase,
        }
    }
}

/// Free a C string allocated by this library
///
/// # Safety
/// Caller must ensure the string was allocated by this library and hasn't been freed
#[no_mangle]
pub unsafe extern "C" fn replicant_string_free(s: *mut c_char) {
    if !s.is_null() {
        let _ = CString::from_raw(s);
    }
}

/// Get the engine's own frozen user UUID
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `out_user_id` - Output pointer for user UUID string (caller must free with replicant_string_free)
///
/// # Returns
/// * SyncResult::Success if the user ID was retrieved
/// * SyncResult::ErrorInvalidInput if engine or out_user_id is null
/// * SyncResult::ErrorDatabase if the user ID could not be read
///
/// # Safety
/// Caller must ensure engine is valid and out_user_id is a valid pointer
#[no_mangle]
pub unsafe extern "C" fn replicant_get_user_id(
    engine: *mut Replicant,
    out_user_id: *mut *mut c_char,
) -> SyncResult {
    if engine.is_null() || out_user_id.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &*engine;

    let user_id = match engine.block_on(async { engine.database.get_user_id().await }) {
        Ok(id) => id,
        Err(_) => return SyncResult::ErrorDatabase,
    };

    match CString::new(user_id.to_string()) {
        Ok(c_str) => {
            *out_user_id = c_str.into_raw();
            SyncResult::Success
        }
        Err(_) => SyncResult::ErrorSerialization,
    }
}

/// Get library version string
#[no_mangle]
pub extern "C" fn replicant_get_version() -> *mut c_char {
    let version = env!("CARGO_PKG_VERSION");
    match CString::new(version) {
        Ok(s) => s.into_raw(),
        Err(_) => ptr::null_mut(),
    }
}

/// Register a callback for document events (Created, Updated, Deleted)
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `callback` - C callback function to invoke for document events
/// * `context` - User-defined context pointer passed to callback
/// * `event_filter` - Optional filter: 0=Created, 1=Updated, 2=Deleted, -1=all document events
///
/// # Returns
/// * SyncResult indicating success or failure
///
/// # Safety
/// Caller must ensure engine is valid, callback is a valid function pointer, and context pointer outlives the callback registration
#[no_mangle]
pub unsafe extern "C" fn replicant_register_document_callback(
    engine: *mut Replicant,
    callback: DocumentEventCallback,
    context: *mut c_void,
    event_filter: i32,
) -> SyncResult {
    if engine.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &*engine;

    let filter = if event_filter >= 0 {
        match event_filter {
            0 => Some(EventType::DocumentCreated),
            1 => Some(EventType::DocumentUpdated),
            2 => Some(EventType::DocumentDeleted),
            _ => return SyncResult::ErrorInvalidInput,
        }
    } else {
        None
    };

    match engine
        .event_dispatcher
        .register_document_callback(callback, context, filter)
    {
        Ok(_) => SyncResult::Success,
        Err(_) => SyncResult::ErrorUnknown,
    }
}

/// Register a callback for sync events (Started, Completed)
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `callback` - C callback function to invoke for sync events
/// * `context` - User-defined context pointer passed to callback
///
/// # Returns
/// * SyncResult indicating success or failure
///
/// # Safety
/// Caller must ensure engine is valid, callback is a valid function pointer, and context pointer outlives the callback registration
#[no_mangle]
pub unsafe extern "C" fn replicant_register_sync_callback(
    engine: *mut Replicant,
    callback: SyncEventCallback,
    context: *mut c_void,
) -> SyncResult {
    if engine.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &*engine;

    match engine
        .event_dispatcher
        .register_sync_callback(callback, context)
    {
        Ok(_) => SyncResult::Success,
        Err(_) => SyncResult::ErrorUnknown,
    }
}

/// Register a callback for error events (SyncError)
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `callback` - C callback function to invoke for error events
/// * `context` - User-defined context pointer passed to callback
///
/// # Returns
/// * SyncResult indicating success or failure
///
/// # Safety
/// Caller must ensure engine is valid, callback is a valid function pointer, and context pointer outlives the callback registration
#[no_mangle]
pub unsafe extern "C" fn replicant_register_error_callback(
    engine: *mut Replicant,
    callback: ErrorEventCallback,
    context: *mut c_void,
) -> SyncResult {
    if engine.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &*engine;

    match engine
        .event_dispatcher
        .register_error_callback(callback, context)
    {
        Ok(_) => SyncResult::Success,
        Err(_) => SyncResult::ErrorUnknown,
    }
}

/// Register a callback for connection events (Lost, Attempted, Succeeded)
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `callback` - C callback function to invoke for connection events
/// * `context` - User-defined context pointer passed to callback
///
/// # Returns
/// * SyncResult indicating success or failure
///
/// # Safety
/// Caller must ensure engine is valid, callback is a valid function pointer, and context pointer outlives the callback registration
#[no_mangle]
pub unsafe extern "C" fn replicant_register_connection_callback(
    engine: *mut Replicant,
    callback: ConnectionEventCallback,
    context: *mut c_void,
) -> SyncResult {
    if engine.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &*engine;

    match engine
        .event_dispatcher
        .register_connection_callback(callback, context)
    {
        Ok(_) => SyncResult::Success,
        Err(_) => SyncResult::ErrorUnknown,
    }
}

/// Register a callback for conflict events (ConflictDetected)
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `callback` - C callback function to invoke for conflict events
/// * `context` - User-defined context pointer passed to callback
///
/// # Returns
/// * SyncResult indicating success or failure
///
/// # Safety
/// Caller must ensure engine is valid, callback is a valid function pointer, and context pointer outlives the callback registration
#[no_mangle]
pub unsafe extern "C" fn replicant_register_conflict_callback(
    engine: *mut Replicant,
    callback: ConflictEventCallback,
    context: *mut c_void,
) -> SyncResult {
    if engine.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &*engine;

    match engine
        .event_dispatcher
        .register_conflict_callback(callback, context)
    {
        Ok(_) => SyncResult::Success,
        Err(_) => SyncResult::ErrorUnknown,
    }
}

/// Register a callback for IdentityChanged events
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `callback` - Function to call when the server-authoritative id is adopted
/// * `context` - User-defined context pointer passed to callback
///
/// # Returns
/// * SyncResult indicating success or failure
///
/// # Safety
/// Caller must ensure engine is valid, callback is a valid function pointer, and context pointer outlives the callback registration
#[no_mangle]
pub unsafe extern "C" fn replicant_register_identity_callback(
    engine: *mut Replicant,
    callback: IdentityEventCallback,
    context: *mut c_void,
) -> SyncResult {
    if engine.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &*engine;

    match engine
        .event_dispatcher
        .register_identity_callback(callback, context)
    {
        Ok(_) => SyncResult::Success,
        Err(_) => SyncResult::ErrorUnknown,
    }
}

/// Process all queued events on the current thread
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `out_processed_count` - Output pointer for number of events processed (optional)
///
/// # Returns
/// * CSyncResult indicating success or failure
///
/// # Important
/// This function MUST be called on the same thread where callbacks were registered.
/// Events are queued from any thread but only processed on the callback thread.
///
/// # Safety
/// Caller must ensure engine is valid and out_processed_count points to valid memory (if not null)
#[no_mangle]
pub unsafe extern "C" fn replicant_process_events(
    engine: *mut Replicant,
    out_processed_count: *mut u32,
) -> SyncResult {
    if engine.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &*engine;

    match engine.event_dispatcher.process_events() {
        Ok(count) => {
            if !out_processed_count.is_null() {
                out_processed_count.write(count as u32);
            }
            SyncResult::Success
        }
        Err(_) => SyncResult::ErrorUnknown,
    }
}

/// Get a document by ID
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `document_id` - Document ID as UUID string
/// * `out_content` - Output pointer for document JSON content (caller must free with replicant_string_free)
///
/// # Returns
/// * SyncResult::Success if document found and content returned
/// * SyncResult::ErrorInvalidInput if document not found or invalid ID
///
/// # Safety
/// Caller must ensure engine is valid, document_id is a valid C string, and out_content is a valid pointer
#[no_mangle]
pub unsafe extern "C" fn replicant_get_document(
    engine: *mut Replicant,
    document_id: *const c_char,
    out_content: *mut *mut c_char,
) -> SyncResult {
    if engine.is_null() || document_id.is_null() || out_content.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &*engine;

    let document_id = match CStr::from_ptr(document_id).to_str() {
        Ok(s) => s,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    let doc_uuid = match Uuid::parse_str(document_id) {
        Ok(id) => id,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    let doc = match engine.block_on(async { engine.database.get_document(&doc_uuid).await }) {
        Ok(d) => d,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    // Serialize document to JSON
    let json = match serde_json::to_string(&doc) {
        Ok(j) => j,
        Err(_) => return SyncResult::ErrorSerialization,
    };

    match CString::new(json) {
        Ok(c_str) => {
            *out_content = c_str.into_raw();
            SyncResult::Success
        }
        Err(_) => SyncResult::ErrorSerialization,
    }
}

/// Get all documents as a JSON array
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `out_documents` - Output pointer for JSON array of documents (caller must free with replicant_string_free)
///
/// # Returns
/// * SyncResult::Success with JSON array (empty array [] if no documents)
///
/// # Safety
/// Caller must ensure engine is valid and out_documents is a valid pointer
#[no_mangle]
pub unsafe extern "C" fn replicant_get_all_documents(
    engine: *mut Replicant,
    out_documents: *mut *mut c_char,
) -> SyncResult {
    if engine.is_null() || out_documents.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &*engine;

    let docs = match engine.block_on(async { engine.database.get_all_documents().await }) {
        Ok(d) => d,
        Err(_) => return SyncResult::ErrorDatabase,
    };

    // Serialize documents array to JSON
    let json = match serde_json::to_string(&docs) {
        Ok(j) => j,
        Err(_) => return SyncResult::ErrorSerialization,
    };

    match CString::new(json) {
        Ok(c_str) => {
            *out_documents = c_str.into_raw();
            SyncResult::Success
        }
        Err(_) => SyncResult::ErrorSerialization,
    }
}

/// Get all document ids as a JSON array
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `include_deleted` - If true, include tombstoned (deleted) documents
/// * `out_ids` - Output pointer for JSON array of id strings (caller must free with replicant_string_free)
///
/// # Returns
/// * SyncResult::Success with JSON array (empty array [] if no documents)
///
/// # Safety
/// Caller must ensure engine is valid and out_ids is a valid pointer
#[no_mangle]
pub unsafe extern "C" fn replicant_get_all_document_ids(
    engine: *mut Replicant,
    include_deleted: bool,
    out_ids: *mut *mut c_char,
) -> SyncResult {
    if engine.is_null() || out_ids.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &*engine;

    let ids = match engine
        .block_on(async { engine.database.get_all_document_ids(include_deleted).await })
    {
        Ok(d) => d,
        Err(_) => return SyncResult::ErrorDatabase,
    };

    // Serialize ids array to JSON
    let json = match serde_json::to_string(&ids) {
        Ok(j) => j,
        Err(_) => return SyncResult::ErrorSerialization,
    };

    match CString::new(json) {
        Ok(c_str) => {
            *out_ids = c_str.into_raw();
            SyncResult::Success
        }
        Err(_) => SyncResult::ErrorSerialization,
    }
}

/// Get the count of local documents
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `out_count` - Output pointer for document count
///
/// # Returns
/// * SyncResult::Success with count written to out_count
///
/// # Safety
/// Caller must ensure engine is valid and out_count is a valid pointer
#[no_mangle]
pub unsafe extern "C" fn replicant_count_documents(
    engine: *mut Replicant,
    out_count: *mut u64,
) -> SyncResult {
    if engine.is_null() || out_count.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &*engine;

    let count = match engine.block_on(async { engine.database.count_documents().await }) {
        Ok(d) => d,
        Err(_) => return SyncResult::ErrorDatabase,
    };

    *out_count = count as u64;
    SyncResult::Success
}

/// Check if the sync engine is connected to the server
///
/// # Arguments
/// * `engine` - Sync engine instance
///
/// # Returns
/// * true if connected, false if disconnected or engine is null
///
/// # Safety
/// Caller must ensure engine was created by replicant_create
#[no_mangle]
pub unsafe extern "C" fn replicant_is_connected(engine: *mut Replicant) -> bool {
    if engine.is_null() {
        return false;
    }

    let engine = &*engine;

    let engine_guard = engine.engine.lock().unwrap();
    match *engine_guard {
        Some(ref sync_engine) => sync_engine.is_connected(),
        None => false,
    }
}

/// Get the count of documents pending sync to server
///
/// # Arguments
/// * `engine` - Sync engine instance
/// * `out_count` - Output pointer for pending document count
///
/// # Returns
/// * SyncResult::Success with count written to out_count
///
/// # Safety
/// Caller must ensure engine is valid and out_count is a valid pointer
#[no_mangle]
pub unsafe extern "C" fn replicant_count_pending_sync(
    engine: *mut Replicant,
    out_count: *mut u64,
) -> SyncResult {
    if engine.is_null() || out_count.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &*engine;

    // If we have a sync engine, use it; otherwise check database directly
    let engine_guard = engine.engine.lock().unwrap();
    let count = if let Some(ref sync_engine) = *engine_guard {
        match engine.block_on(async { sync_engine.count_pending_sync().await }) {
            Ok(c) => c,
            Err(_) => return SyncResult::ErrorDatabase,
        }
    } else {
        drop(engine_guard);
        // Offline mode - check pending documents in database
        match engine.block_on(async { engine.database.get_pending_documents().await }) {
            Ok(docs) => docs.len(),
            Err(_) => return SyncResult::ErrorDatabase,
        }
    };

    *out_count = count as u64;
    SyncResult::Success
}

// ===== FTS (Full-Text Search) Functions =====

/// Configure which JSON paths to index for full-text search
///
/// # Arguments
/// * `engine` - Replicant client instance
/// * `paths_json` - JSON array of JSON paths to index (e.g., '["$.body", "$.notes"]')
///
/// # Returns
/// * SyncResult::Success if configuration succeeded
/// * SyncResult::ErrorInvalidInput if paths_json is invalid
/// * SyncResult::ErrorDatabase if index rebuild fails
///
/// # Note
/// This replaces any existing configuration and rebuilds the search index.
///
/// # Safety
/// Caller must ensure engine is valid and paths_json is a valid C string
#[no_mangle]
pub unsafe extern "C" fn replicant_configure_search(
    engine: *mut Replicant,
    paths_json: *const c_char,
) -> SyncResult {
    if engine.is_null() || paths_json.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &*engine;

    let paths_json = match CStr::from_ptr(paths_json).to_str() {
        Ok(s) => s,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    // Parse JSON array of paths
    let paths: Vec<String> = match serde_json::from_str(paths_json) {
        Ok(p) => p,
        Err(_) => return SyncResult::ErrorSerialization,
    };

    match engine.block_on(async { engine.database.configure_search(&paths).await }) {
        Ok(_) => SyncResult::Success,
        Err(_) => SyncResult::ErrorDatabase,
    }
}

/// Search documents using FTS5 full-text search
///
/// # Arguments
/// * `engine` - Replicant client instance
/// * `query` - FTS5 query string (e.g., "music", "tun*", "\"exact phrase\"")
/// * `limit` - Maximum number of results (0 for default of 100)
/// * `out_documents` - Output pointer for JSON array of matching documents
///
/// # Returns
/// * SyncResult::Success with JSON array in out_documents
/// * SyncResult::ErrorInvalidInput if query is invalid
/// * SyncResult::ErrorDatabase if search fails
///
/// # FTS5 Query Syntax
/// * Simple terms: "music" matches documents containing "music"
/// * Prefix: "tun*" matches "tuning", "tune", etc.
/// * Phrase: "\"equal temperament\"" matches exact phrase
/// * Boolean: "music AND theory", "piano OR keyboard"
/// * Column filter: "title:beethoven" searches only title field
///
/// # Safety
/// Caller must ensure engine is valid, query is a valid C string,
/// and out_documents is a valid pointer. Caller must free result with replicant_string_free.
#[no_mangle]
pub unsafe extern "C" fn replicant_search_documents(
    engine: *mut Replicant,
    query: *const c_char,
    limit: u32,
    out_documents: *mut *mut c_char,
) -> SyncResult {
    if engine.is_null() || query.is_null() || out_documents.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &*engine;

    let query = match CStr::from_ptr(query).to_str() {
        Ok(s) => s,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    let limit = if limit == 0 { 100 } else { limit as i64 };

    let docs = match engine.block_on(async { engine.database.search_documents(query, limit).await })
    {
        Ok(d) => d,
        Err(_) => return SyncResult::ErrorDatabase,
    };

    // Serialize documents array to JSON
    let json = match serde_json::to_string(&docs) {
        Ok(j) => j,
        Err(_) => return SyncResult::ErrorSerialization,
    };

    match CString::new(json) {
        Ok(c_str) => {
            *out_documents = c_str.into_raw();
            SyncResult::Success
        }
        Err(_) => SyncResult::ErrorSerialization,
    }
}

/// Rebuild the full-text search index
///
/// # Arguments
/// * `engine` - Replicant client instance
///
/// # Returns
/// * SyncResult::Success if rebuild succeeded
/// * SyncResult::ErrorDatabase if rebuild fails
///
/// # Note
/// This is called automatically by replicant_configure_search, but can be
/// called manually if needed (e.g., after bulk document imports).
///
/// # Safety
/// Caller must ensure engine is valid
#[no_mangle]
pub unsafe extern "C" fn replicant_rebuild_search_index(engine: *mut Replicant) -> SyncResult {
    if engine.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let engine = &*engine;

    match engine.block_on(async { engine.database.rebuild_fts_index().await }) {
        Ok(_) => SyncResult::Success,
        Err(_) => SyncResult::ErrorDatabase,
    }
}

// ============================================================================
// Enrollment + credential storage
// ============================================================================

/// Copies `s` plus a NUL terminator into `out` iff it fits within `cap`
/// bytes. Returns `false` — writing an empty C string when `cap > 0` — when
/// it does not fit; never writes past `cap`. On a multi-buffer call that
/// fails partway, earlier out buffers may already be populated — callers
/// must not read any out buffer unless the call returned success. Bytes of
/// `s` are copied verbatim, so an embedded NUL makes C readers see the
/// string truncated at that NUL.
///
/// # Safety
/// `out` must point to a writable buffer of at least `cap` bytes.
unsafe fn write_cstr_buf(out: *mut c_char, cap: usize, s: &str) -> bool {
    if cap == 0 {
        return false;
    }
    let bytes = s.as_bytes();
    if bytes.len() + 1 > cap {
        out.write(0);
        return false;
    }
    ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, bytes.len());
    out.add(bytes.len()).write(0);
    true
}

/// Requests an enrollment token be emailed to `email`. Standalone HTTP call
/// (no engine handle); runs on a dedicated thread with its own short-lived
/// runtime so this is safe to call even from inside an async runtime context.
///
/// BLOCKING: waits for the HTTP round-trip (connect ~10s / request ~30s
/// timeouts). TODO(#40): add a completion-callback async variant
/// (`replicant_enroll_request_async`) so consumers don't block a caller thread.
///
/// # Safety
/// `base_url` and `email` must be valid, non-null C strings.
#[no_mangle]
pub unsafe extern "C" fn replicant_enroll_request(
    base_url: *const c_char,
    email: *const c_char,
) -> SyncResult {
    if base_url.is_null() || email.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let base_url = match CStr::from_ptr(base_url).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return SyncResult::ErrorInvalidInput,
    };
    let email = match CStr::from_ptr(email).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    let join_result = std::thread::spawn(move || {
        let runtime = Runtime::new().map_err(|_| SyncResult::ErrorUnknown)?;
        match runtime.block_on(crate::enrollment::request(&base_url, &email)) {
            Ok(()) => Ok(()),
            Err(_) => Err(SyncResult::ErrorConnection),
        }
    })
    .join();

    match join_result {
        Ok(Ok(())) => SyncResult::Success,
        Ok(Err(err)) => err,
        Err(_) => SyncResult::ErrorUnknown,
    }
}

/// Exchanges an enrollment token for a per-user credential. On success writes
/// the api_key, secret, and canonical user id (36-char UUID string) into the
/// out buffers; each `*_cap` is the writable size of its buffer in bytes and
/// the call fails (without overflowing) when a value does not fit.
///
/// BLOCKING: waits for the HTTP round-trip (connect ~10s / request ~30s
/// timeouts). TODO(#40): add a completion-callback async variant
/// (`replicant_enroll_claim_async`) so consumers don't block a caller thread.
///
/// # Safety
/// All string pointers must be valid, non-null C strings; each out pointer
/// must reference a writable buffer of at least its stated capacity.
#[no_mangle]
pub unsafe extern "C" fn replicant_enroll_claim(
    base_url: *const c_char,
    email: *const c_char,
    token: *const c_char,
    out_api_key: *mut c_char,
    api_key_cap: usize,
    out_secret: *mut c_char,
    secret_cap: usize,
    out_user_id: *mut c_char,
    user_id_cap: usize,
) -> SyncResult {
    if base_url.is_null()
        || email.is_null()
        || token.is_null()
        || out_api_key.is_null()
        || out_secret.is_null()
        || out_user_id.is_null()
    {
        return SyncResult::ErrorInvalidInput;
    }

    let base_url = match CStr::from_ptr(base_url).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return SyncResult::ErrorInvalidInput,
    };
    let email = match CStr::from_ptr(email).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return SyncResult::ErrorInvalidInput,
    };
    let token = match CStr::from_ptr(token).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    let join_result = std::thread::spawn(move || {
        let runtime = Runtime::new().map_err(|_| SyncResult::ErrorUnknown)?;
        runtime
            .block_on(crate::enrollment::claim(&base_url, &email, &token))
            .map_err(|e| match e {
                crate::enrollment::EnrollError::InvalidToken => SyncResult::ErrorInvalidInput,
                // A malformed/incomplete 200 is a bad response, not a transport
                // failure — keep it distinct from network errors so callers can
                // tell "your code is wrong" from "the server misbehaved".
                crate::enrollment::EnrollError::InvalidResponse => SyncResult::ErrorSerialization,
                _ => SyncResult::ErrorConnection,
            })
    })
    .join();

    match join_result {
        Ok(Ok(creds)) => {
            if write_cstr_buf(out_api_key, api_key_cap, &creds.api_key)
                && write_cstr_buf(out_secret, secret_cap, &creds.secret)
                && write_cstr_buf(out_user_id, user_id_cap, &creds.user_id.to_string())
            {
                SyncResult::Success
            } else {
                SyncResult::ErrorInvalidInput
            }
        }
        Ok(Err(err)) => err,
        Err(_) => SyncResult::ErrorUnknown,
    }
}

/// Loads stored credentials from `data_dir`. Returns Success and fills the
/// out buffers (api_key, secret, canonical user id), or ErrorDatabase if none
/// are stored / unreadable. Each `*_cap` is the writable size of its buffer;
/// the call fails (without overflowing) when a value does not fit.
///
/// # Safety
/// `data_dir` must be a valid, non-null C string; each out pointer must
/// reference a writable buffer of at least its stated capacity.
#[no_mangle]
pub unsafe extern "C" fn replicant_load_credentials(
    data_dir: *const c_char,
    out_api_key: *mut c_char,
    api_key_cap: usize,
    out_secret: *mut c_char,
    secret_cap: usize,
    out_user_id: *mut c_char,
    user_id_cap: usize,
) -> SyncResult {
    if data_dir.is_null() || out_api_key.is_null() || out_secret.is_null() || out_user_id.is_null()
    {
        return SyncResult::ErrorInvalidInput;
    }

    let data_dir = match CStr::from_ptr(data_dir).to_str() {
        Ok(s) => s,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    match crate::secret_store::load(std::path::Path::new(data_dir)) {
        Ok(Some(creds)) => {
            if write_cstr_buf(out_api_key, api_key_cap, &creds.api_key)
                && write_cstr_buf(out_secret, secret_cap, &creds.secret)
                && write_cstr_buf(out_user_id, user_id_cap, &creds.user_id.to_string())
            {
                SyncResult::Success
            } else {
                SyncResult::ErrorInvalidInput
            }
        }
        Ok(None) | Err(_) => SyncResult::ErrorDatabase,
    }
}

/// Stores credentials to `data_dir` (encrypted at rest). `user_id` is the
/// canonical id delivered by enrollment claim (36-char UUID string); a nil or
/// unparseable id is rejected — credentials are never stored without a real
/// identity.
///
/// # Safety
/// All pointers must be valid, non-null C strings.
#[no_mangle]
pub unsafe extern "C" fn replicant_store_credentials(
    data_dir: *const c_char,
    api_key: *const c_char,
    secret: *const c_char,
    user_id: *const c_char,
) -> SyncResult {
    if data_dir.is_null() || api_key.is_null() || secret.is_null() || user_id.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let data_dir = match CStr::from_ptr(data_dir).to_str() {
        Ok(s) => s,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };
    let api_key = match CStr::from_ptr(api_key).to_str() {
        Ok(s) => s,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };
    let secret = match CStr::from_ptr(secret).to_str() {
        Ok(s) => s,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };
    let user_id = match CStr::from_ptr(user_id)
        .to_str()
        .ok()
        .and_then(|s| Uuid::parse_str(s).ok())
    {
        Some(id) if !id.is_nil() => id,
        _ => return SyncResult::ErrorInvalidInput,
    };

    let creds = crate::secret_store::Credentials {
        api_key: api_key.to_string(),
        secret: secret.to_string(),
        user_id,
    };

    match crate::secret_store::store(std::path::Path::new(data_dir), &creds) {
        Ok(()) => SyncResult::Success,
        Err(_) => SyncResult::ErrorDatabase,
    }
}

/// Clears any stored credentials in `data_dir`.
///
/// # Safety
/// `data_dir` must be a valid, non-null C string.
#[no_mangle]
pub unsafe extern "C" fn replicant_clear_credentials(data_dir: *const c_char) -> SyncResult {
    if data_dir.is_null() {
        return SyncResult::ErrorInvalidInput;
    }

    let data_dir = match CStr::from_ptr(data_dir).to_str() {
        Ok(s) => s,
        Err(_) => return SyncResult::ErrorInvalidInput,
    };

    match crate::secret_store::clear(std::path::Path::new(data_dir)) {
        Ok(()) => SyncResult::Success,
        Err(_) => SyncResult::ErrorDatabase,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_cstr_buf_refuses_oversized_strings() {
        let mut small = [1i8; 8];
        let fitted = unsafe {
            write_cstr_buf(
                small.as_mut_ptr() as *mut c_char,
                small.len(),
                "too-long-for-8",
            )
        };
        assert!(!fitted);
        assert_eq!(small[0], 0, "refused write must leave an empty C string");

        let mut big = [1i8; 32];
        let fitted = unsafe { write_cstr_buf(big.as_mut_ptr() as *mut c_char, big.len(), "fits") };
        assert!(fitted);
        assert_eq!(big[4], 0, "NUL terminator after the copied bytes");
    }

    #[test]
    fn write_cstr_buf_exact_fit_boundary() {
        // len + 1 == cap fits exactly; len == cap does not.
        let mut buf = [1i8; 5];
        assert!(unsafe { write_cstr_buf(buf.as_mut_ptr() as *mut c_char, buf.len(), "four") });
        assert_eq!(buf[4], 0);
        assert!(!unsafe { write_cstr_buf(buf.as_mut_ptr() as *mut c_char, buf.len(), "five!") });
        assert_eq!(buf[0], 0, "refused write must leave an empty C string");
    }

    #[test]
    fn write_cstr_buf_zero_capacity_is_refused() {
        let mut buf = [1i8; 1];
        assert!(!unsafe { write_cstr_buf(buf.as_mut_ptr() as *mut c_char, 0, "") });
        assert_eq!(buf[0], 1, "zero-cap buffer must not be touched");
    }

    #[tokio::test]
    async fn enroll_ffi_is_callable_from_within_a_runtime() {
        // Pre-guard code panicked here ("Cannot start a runtime from within a
        // runtime"); the OS-thread hop makes this safe. The insecure URL makes
        // the call fail fast without any network traffic.
        let url = CString::new("http://example.com").unwrap();
        let email = CString::new("rt@test.com").unwrap();
        let result = unsafe { replicant_enroll_request(url.as_ptr(), email.as_ptr()) };
        assert_ne!(result, SyncResult::Success);

        let token = CString::new("TOK").unwrap();
        let mut key = [0i8; 129];
        let mut secret = [0i8; 129];
        let mut uid = [0i8; 37];
        let result = unsafe {
            replicant_enroll_claim(
                url.as_ptr(),
                email.as_ptr(),
                token.as_ptr(),
                key.as_mut_ptr() as *mut c_char,
                key.len(),
                secret.as_mut_ptr() as *mut c_char,
                secret.len(),
                uid.as_mut_ptr() as *mut c_char,
                uid.len(),
            )
        };
        assert_ne!(result, SyncResult::Success);
    }
}
