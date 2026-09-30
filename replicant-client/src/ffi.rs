//! C API. A handle is one attachment to the engine for its data dir; every handle in the
//! process on that data dir shares one engine (`host::attach`). No call waits on the network.

use std::ffi::{c_void, CStr, CString};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::runtime::Runtime;
use uuid::Uuid;

use crate::engine::list_merge::{ListMergeConfig, ListMergePolicy, PathPattern};
use crate::engine::machine::{ConnectionView, EngineState, HaltReason, SyncView};
use crate::events::{
    ConflictEventCallback, ConnectionEventCallback, DispatchError, Dispatcher,
    DocumentEventCallback, ErrorEventCallback, EventType, SyncEventCallback,
};
use crate::host::{self, Handle, HostConfig, OpenError};
use crate::store::StoreError;

/// Opaque handle. Every call except `replicant_process_events` is thread-safe. Never call from an
/// audio thread: every read and write is a SQLite transaction.
pub struct Replicant {
    handle: Handle,
    dispatcher: Dispatcher,
    /// `PUMP_*`: whether `replicant_process_events` is running callbacks, and whether a
    /// callback asked to destroy the handle meanwhile.
    pump: AtomicU8,
}

const PUMP_IDLE: u8 = 0;
const PUMP_DISPATCHING: u8 = 1;
const PUMP_DESTROY_REQUESTED: u8 = 2;

/// The version of this C ABI: bumped on any change to a struct layout, enum value or
/// signature. A host compares it with `REPLICANT_ABI_VERSION` from the header it compiled.
pub const REPLICANT_ABI_VERSION: i32 = 1;

#[no_mangle]
pub extern "C" fn replicant_abi_version() -> i32 {
    REPLICANT_ABI_VERSION
}

/// Every entry point's body runs in here: a panic becomes `ErrorUnknown` instead of unwinding
/// into C, which would abort the host (a DAW).
fn guard(body: impl FnOnce() -> SyncResult) -> SyncResult {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or_else(|_| {
        tracing::error!("a replicant call panicked");
        SyncResult::ErrorUnknown
    })
}

/// cbindgen:prefix-with-name
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncResult {
    Success = 0,
    ErrorInvalidInput = -1,
    /// Enrollment only: the server could not be reached.
    ErrorConnection = -2,
    ErrorDatabase = -3,
    ErrorSerialization = -4,
    /// A newer build migrated the database; this build cannot open it.
    ErrorNewerSchema = -5,
    /// An engine is open on this data dir with a different server, host or list merge config.
    /// A programming error: every binary on a data dir must pass the same config. Never fall
    /// back to a temporary library.
    ErrorConfigMismatch = -6,
    ErrorNotFound = -7,
    /// Another account's document, or a read-only publication.
    ErrorNotWritable = -8,
    /// The id exists here or was deleted. An import should skip the id, not count a failure.
    ErrorAlreadyExists = -9,
    /// Migrating a v1 library failed; nothing was changed and a `.v1-backup` copy sits next to
    /// the database. Show "Your library needs attention"; never fall back to a temporary library.
    ErrorMigrationFailed = -10,
    /// Another process kept the database locked; try `replicant_create` again shortly.
    ErrorBusy = -11,
    /// `replicant_process_events` was called on another thread than the one that registered.
    ErrorWrongThread = -12,
    /// `replicant_process_events` was called before any callback was registered.
    ErrorNoCallbacks = -13,
    /// The kept copy's document was deleted: `replicant_restore_document` re-creates the copy.
    ErrorDocumentGone = -14,
    ErrorUnknown = -99,
}

/// Every string is UTF-8 and copied by `replicant_create`.
#[repr(C)]
pub struct ReplicantConfig {
    /// `sizeof(ReplicantConfig)`; any other value is refused (`ErrorInvalidInput`), so a later
    /// version can add fields without reading past an older host's struct.
    pub struct_size: u32,
    /// Holds the database file and the stored credentials.
    pub data_dir: *const c_char,
    /// File name inside `data_dir`, e.g. "tonaldb.sqlite3".
    pub database_file: *const c_char,
    pub server_url: *const c_char,
    /// May be null. Signs joins when the stored credentials carry no email.
    pub email: *const c_char,
    /// Named in the User-Agent, e.g. "Entonal Studio" and "2.0.1 CLAP".
    pub host_app: *const c_char,
    pub host_version: *const c_char,
    /// A `ReplicantListMerge`: how a list both sides changed is merged when no rule matches.
    /// An `int32_t` so that a value from C outside the enum is refused, not undefined.
    pub list_merge: i32,
    /// May be null. A JSON array of `{"path": "/pitches", "policy": "atomic"}`;
    /// policies `append`, `atomic`, `full`. `*` matches one key or index; the rule with the most
    /// literal segments wins, a tie goes to the first listed.
    pub list_merge_rules_json: *const c_char,
}

/// Merge policy for a list changed on both sides. `ListMergeFull` is refused for now.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicantListMerge {
    /// Element by element while positions line up on both sides; otherwise the server's list is
    /// kept and the local one set aside.
    ListMergeAppend = 0,
    /// Any change on both sides keeps the server's list and sets the local one aside.
    ListMergeAtomic = 1,
    ListMergeFull = 2,
}

#[derive(serde::Deserialize)]
struct ListMergeRule {
    path: String,
    policy: String,
}

/// `value` is a `ReplicantListMerge`.
fn list_merge_policy(value: i32) -> Option<ListMergePolicy> {
    match value {
        0 => Some(ListMergePolicy::Append),
        1 => Some(ListMergePolicy::Atomic),
        2 => Some(ListMergePolicy::Full),
        _ => None,
    }
}

fn list_merge_policy_named(name: &str) -> Option<ListMergePolicy> {
    match name {
        "append" => Some(ListMergePolicy::Append),
        "atomic" => Some(ListMergePolicy::Atomic),
        "full" => Some(ListMergePolicy::Full),
        _ => None,
    }
}

/// `None` for an unknown policy or unparsable rules; `Engine::start` validates the rest.
unsafe fn list_merge_arg(config: &ReplicantConfig) -> Option<ListMergeConfig> {
    let default = list_merge_policy(config.list_merge)?;
    let rules = if config.list_merge_rules_json.is_null() {
        Vec::new()
    } else {
        let rules: Vec<ListMergeRule> =
            serde_json::from_str(str_arg(config.list_merge_rules_json)?).ok()?;
        rules
            .into_iter()
            .map(|rule| {
                Some((
                    PathPattern(rule.path),
                    list_merge_policy_named(&rule.policy)?,
                ))
            })
            .collect::<Option<Vec<_>>>()?
    };
    Some(ListMergeConfig { default, rules })
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicantConnection {
    ConnectionIdle = 0,
    ConnectionDisconnected = 1,
    ConnectionConnecting = 2,
    ConnectionConnected = 3,
    /// Not retrying on its own; see `halt_reason`.
    ConnectionHalted = 4,
    ConnectionStopped = 5,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicantSync {
    SyncIdle = 0,
    SyncCatchingUp = 1,
    SyncLive = 2,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicantHaltReason {
    HaltNone = 0,
    /// No stored credentials: sign in.
    HaltNotEnrolled = 1,
    /// The server refused the credentials: sign in again.
    HaltAuthInvalid = 2,
    /// The server needs a newer client.
    HaltUpdateRequired = 3,
    HaltAccountDisabled = 4,
    /// This data dir belongs to another account.
    HaltIdentityDrift = 5,
    HaltOther = 6,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicantState {
    /// Set to `sizeof(ReplicantState)` before `replicant_get_state`; any other value is
    /// refused, so a later version never writes past an older host's struct.
    pub struct_size: u32,
    pub connection: ReplicantConnection,
    pub sync: ReplicantSync,
    pub halt_reason: ReplicantHaltReason,
}

impl From<&EngineState> for ReplicantState {
    fn from(state: &EngineState) -> Self {
        let (connection, halt_reason) = match &state.connection {
            ConnectionView::Idle => (
                ReplicantConnection::ConnectionIdle,
                ReplicantHaltReason::HaltNone,
            ),
            ConnectionView::Disconnected => (
                ReplicantConnection::ConnectionDisconnected,
                ReplicantHaltReason::HaltNone,
            ),
            ConnectionView::Connecting => (
                ReplicantConnection::ConnectionConnecting,
                ReplicantHaltReason::HaltNone,
            ),
            ConnectionView::Connected => (
                ReplicantConnection::ConnectionConnected,
                ReplicantHaltReason::HaltNone,
            ),
            ConnectionView::Stopped => (
                ReplicantConnection::ConnectionStopped,
                ReplicantHaltReason::HaltNone,
            ),
            ConnectionView::Halted(reason) => (
                ReplicantConnection::ConnectionHalted,
                match reason {
                    HaltReason::NotEnrolled => ReplicantHaltReason::HaltNotEnrolled,
                    HaltReason::AuthInvalid => ReplicantHaltReason::HaltAuthInvalid,
                    HaltReason::UpdateRequired => ReplicantHaltReason::HaltUpdateRequired,
                    HaltReason::AccountDisabled => ReplicantHaltReason::HaltAccountDisabled,
                    HaltReason::Other(code) if code == "identity_drift" => {
                        ReplicantHaltReason::HaltIdentityDrift
                    }
                    HaltReason::Other(_) => ReplicantHaltReason::HaltOther,
                },
            ),
        };
        let sync = match state.sync {
            SyncView::Idle => ReplicantSync::SyncIdle,
            SyncView::CatchingUp => ReplicantSync::SyncCatchingUp,
            SyncView::Live => ReplicantSync::SyncLive,
        };
        ReplicantState {
            struct_size: std::mem::size_of::<ReplicantState>() as u32,
            connection,
            sync,
            halt_reason,
        }
    }
}

unsafe fn str_arg<'a>(value: *const c_char) -> Option<&'a str> {
    if value.is_null() {
        None
    } else {
        CStr::from_ptr(value).to_str().ok()
    }
}

/// Null is `None`; a non-null string that is not UTF-8 is refused.
unsafe fn nullable_str_arg<'a>(value: *const c_char) -> Result<Option<&'a str>, SyncResult> {
    if value.is_null() {
        Ok(None)
    } else {
        str_arg(value)
            .map(Some)
            .ok_or(SyncResult::ErrorInvalidInput)
    }
}

unsafe fn uuid_arg(value: *const c_char) -> Option<Uuid> {
    str_arg(value).and_then(|text| Uuid::parse_str(text).ok())
}

unsafe fn json_arg(value: *const c_char) -> Result<Value, SyncResult> {
    let text = str_arg(value).ok_or(SyncResult::ErrorInvalidInput)?;
    serde_json::from_str(text).map_err(|_| SyncResult::ErrorSerialization)
}

fn store_result(error: &StoreError) -> SyncResult {
    match error {
        StoreError::NotFound(_) | StoreError::NoFieldConflict(_) | StoreError::NoKeptCopy(_) => {
            SyncResult::ErrorNotFound
        }
        StoreError::NotWritable(_) => SyncResult::ErrorNotWritable,
        StoreError::DocumentGone(_) => SyncResult::ErrorDocumentGone,
        StoreError::AlreadyExists(_) => SyncResult::ErrorAlreadyExists,
        StoreError::Json(_) => SyncResult::ErrorSerialization,
        StoreError::BadSearchQuery(_) => SyncResult::ErrorInvalidInput,
        _ => SyncResult::ErrorDatabase,
    }
}

/// Writes the 36-character id and a NUL; `out` must hold 37 bytes.
unsafe fn write_id(out: *mut c_char, id: Uuid) {
    let text = id.to_string();
    ptr::copy_nonoverlapping(text.as_ptr(), out as *mut u8, text.len());
    out.add(text.len()).write(0);
}

/// Hands `value` to the caller as JSON; free it with `replicant_string_free`.
unsafe fn write_json(out: *mut *mut c_char, value: &impl Serialize) -> SyncResult {
    match serde_json::to_string(value)
        .ok()
        .and_then(|json| CString::new(json).ok())
    {
        Some(json) => {
            *out = json.into_raw();
            SyncResult::Success
        }
        None => SyncResult::ErrorSerialization,
    }
}

/// Attaches to the engine for `config`'s data dir, starting it if this process has none yet.
/// Opens (and after an upgrade migrates) the database on this thread; never waits on the
/// network. On success `*out_handle` is set; otherwise it is null. Call `replicant_get_state`
/// next: a handle attached to an engine that is already halted is not sent the halt again.
///
/// # Safety
/// `config` and `out_handle` must be valid; the config's strings valid C strings (`email` may be null).
#[no_mangle]
pub unsafe extern "C" fn replicant_create(
    config: *const ReplicantConfig,
    out_handle: *mut *mut Replicant,
) -> SyncResult {
    guard(|| {
        if config.is_null() || out_handle.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        *out_handle = ptr::null_mut();
        let config = &*config;
        if config.struct_size as usize != std::mem::size_of::<ReplicantConfig>() {
            return SyncResult::ErrorInvalidInput;
        }
        let (
            Some(data_dir),
            Some(database_file),
            Some(server_url),
            Some(host_app),
            Some(host_version),
        ) = (
            str_arg(config.data_dir),
            str_arg(config.database_file),
            str_arg(config.server_url),
            str_arg(config.host_app),
            str_arg(config.host_version),
        )
        else {
            return SyncResult::ErrorInvalidInput;
        };
        let Some(list_merge) = list_merge_arg(config) else {
            return SyncResult::ErrorInvalidInput;
        };
        let host_config = HostConfig {
            data_dir: PathBuf::from(data_dir),
            database_file: database_file.to_string(),
            server_url: server_url.to_string(),
            fallback_email: str_arg(config.email).map(str::to_string),
            host_app: host_app.to_string(),
            host_version: host_version.to_string(),
            list_merge,
        };
        match host::attach(host_config) {
            Ok(handle) => {
                *out_handle = Box::into_raw(Box::new(Replicant {
                    handle,
                    dispatcher: Dispatcher::default(),
                    pump: AtomicU8::new(PUMP_IDLE),
                }));
                SyncResult::Success
            }
            Err(OpenError::NewerSchema) => SyncResult::ErrorNewerSchema,
            Err(OpenError::MigrationFailed(message)) => {
                tracing::error!(%message, "replicant_create: v1 migration failed");
                SyncResult::ErrorMigrationFailed
            }
            Err(OpenError::Busy) => SyncResult::ErrorBusy,
            Err(OpenError::ConfigMismatch(_)) => SyncResult::ErrorConfigMismatch,
            Err(OpenError::Config(message)) => {
                tracing::warn!(%message, "replicant_create: invalid config");
                SyncResult::ErrorInvalidInput
            }
            Err(error) => {
                tracing::warn!(%error, "replicant_create failed");
                SyncResult::ErrorDatabase
            }
        }
    })
}

/// `replicant_process_events` is running this handle's callbacks: it frees the handle when
/// it returns. True when the free was handed over.
unsafe fn defer_to_the_pump(handle: *mut Replicant) -> bool {
    (*handle)
        .pump
        .compare_exchange(
            PUMP_DISPATCHING,
            PUMP_DESTROY_REQUESTED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_ok()
}

unsafe fn free(handle: *mut Replicant) {
    let replicant = Box::from_raw(handle);
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let Replicant { handle, .. } = *replicant;
        drop(handle.close());
    }))
    .is_err()
    {
        tracing::error!("replicant_destroy panicked");
    }
}

/// Detaches and frees the handle. Returns at once: when this was the engine's last handle, the
/// engine stops on a Replicant thread afterwards (never waiting on the network). Called from
/// inside one of this handle's callbacks, the free happens when `replicant_process_events`
/// returns.
///
/// Unloading: library code can still run after this returns, and even after
/// `replicant_destroy_and_wait` returns true, because a DNS lookup the engine started finishes
/// on its own thread, bounded only by the OS resolver. Never unload this library while the
/// process runs: a plugin pins its module (`RTLD_NODELETE`, or
/// `GET_MODULE_HANDLE_EX_FLAG_PIN` on Windows).
///
/// # Safety
/// `handle` must come from `replicant_create` and not be used again.
#[no_mangle]
pub unsafe extern "C" fn replicant_destroy(handle: *mut Replicant) {
    if handle.is_null() || defer_to_the_pump(handle) {
        return;
    }
    free(handle);
}

/// `replicant_destroy`, then waits up to `timeout_ms` for the engine to stop and its runtime to
/// shut down. True when it has, or when other handles keep the engine running. It does not make
/// unloading safe (see `replicant_destroy`). From inside a
/// callback it cannot wait: it returns false and the handle is freed when the pump returns.
///
/// # Safety
/// As `replicant_destroy`.
#[no_mangle]
pub unsafe extern "C" fn replicant_destroy_and_wait(
    handle: *mut Replicant,
    timeout_ms: u32,
) -> bool {
    if handle.is_null() {
        return true;
    }
    if defer_to_the_pump(handle) {
        return false;
    }
    let replicant = Box::from_raw(handle);
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let Replicant { handle, .. } = *replicant;
        handle
            .close()
            .wait(Duration::from_millis(u64::from(timeout_ms)))
    }))
    .unwrap_or(false)
}

/// Creates a document owned by the signed-in (or provisional) user; writes its id into
/// `out_document_id` (37 bytes).
///
/// # Safety
/// Valid handle, C string, and a 37-byte buffer.
#[no_mangle]
pub unsafe extern "C" fn replicant_create_document(
    handle: *mut Replicant,
    content_json: *const c_char,
    out_document_id: *mut c_char,
) -> SyncResult {
    guard(|| {
        if handle.is_null() || out_document_id.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        let content = match json_arg(content_json) {
            Ok(content) => content,
            Err(result) => return result,
        };
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant
            .handle
            .block_on(store.create_document(None, content))
        {
            Ok(doc_id) => {
                replicant.handle.notify_outbox();
                write_id(out_document_id, doc_id);
                SyncResult::Success
            }
            Err(error) => store_result(&error),
        }
    })
}

/// Creates a document with a chosen id. Refused (`ErrorAlreadyExists`) when the id exists here
/// or was deleted.
///
/// # Safety
/// Valid handle and C strings.
#[no_mangle]
pub unsafe extern "C" fn replicant_create_document_with_id(
    handle: *mut Replicant,
    document_id: *const c_char,
    content_json: *const c_char,
) -> SyncResult {
    guard(|| {
        let (false, Some(doc_id)) = (handle.is_null(), uuid_arg(document_id)) else {
            return SyncResult::ErrorInvalidInput;
        };
        let content = match json_arg(content_json) {
            Ok(content) => content,
            Err(result) => return result,
        };
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant
            .handle
            .block_on(store.create_document(Some(doc_id), content))
        {
            Ok(_) => {
                replicant.handle.notify_outbox();
                SyncResult::Success
            }
            Err(error) => store_result(&error),
        }
    })
}

/// # Safety
/// Valid handle and C strings.
#[no_mangle]
pub unsafe extern "C" fn replicant_update_document(
    handle: *mut Replicant,
    document_id: *const c_char,
    content_json: *const c_char,
) -> SyncResult {
    guard(|| {
        let (false, Some(doc_id)) = (handle.is_null(), uuid_arg(document_id)) else {
            return SyncResult::ErrorInvalidInput;
        };
        let content = match json_arg(content_json) {
            Ok(content) => content,
            Err(result) => return result,
        };
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant
            .handle
            .block_on(store.update_document(doc_id, content))
        {
            Ok(()) => {
                replicant.handle.notify_outbox();
                SyncResult::Success
            }
            Err(error) => store_result(&error),
        }
    })
}

/// # Safety
/// Valid handle and C string.
#[no_mangle]
pub unsafe extern "C" fn replicant_delete_document(
    handle: *mut Replicant,
    document_id: *const c_char,
) -> SyncResult {
    guard(|| {
        let (false, Some(doc_id)) = (handle.is_null(), uuid_arg(document_id)) else {
            return SyncResult::ErrorInvalidInput;
        };
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant.handle.block_on(store.delete_document(doc_id)) {
            Ok(()) => {
                replicant.handle.notify_outbox();
                SyncResult::Success
            }
            Err(error) => store_result(&error),
        }
    })
}

/// The document as JSON: `id`, `user_id` (owner), `author_id`, `title`, `content`, `read_only`,
/// `visibility` (`public`/`private`), `source_doc_id`, `derived_from`, `created_at`,
/// `updated_at`. `ErrorNotFound` when missing or deleted.
///
/// # Safety
/// Valid handle, C string and out pointer; free the result with `replicant_string_free`.
#[no_mangle]
pub unsafe extern "C" fn replicant_get_document(
    handle: *mut Replicant,
    document_id: *const c_char,
    out_json: *mut *mut c_char,
) -> SyncResult {
    guard(|| {
        let (false, false, Some(doc_id)) =
            (handle.is_null(), out_json.is_null(), uuid_arg(document_id))
        else {
            return SyncResult::ErrorInvalidInput;
        };
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant.handle.block_on(store.get_document(doc_id)) {
            Ok(Some(document)) => write_json(out_json, &document),
            Ok(None) => SyncResult::ErrorNotFound,
            Err(error) => store_result(&error),
        }
    })
}

/// Every visible document as a JSON array (see `replicant_get_document`).
///
/// # Safety
/// Valid handle and out pointer; free the result with `replicant_string_free`.
#[no_mangle]
pub unsafe extern "C" fn replicant_get_all_documents(
    handle: *mut Replicant,
    out_json: *mut *mut c_char,
) -> SyncResult {
    guard(|| {
        if handle.is_null() || out_json.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant.handle.block_on(store.list_documents()) {
            Ok(documents) => write_json(out_json, &documents),
            Err(error) => store_result(&error),
        }
    })
}

/// Document ids as a JSON array; `include_deleted` adds documents whose delete is not yet sent.
///
/// # Safety
/// Valid handle and out pointer; free the result with `replicant_string_free`.
#[no_mangle]
pub unsafe extern "C" fn replicant_get_all_document_ids(
    handle: *mut Replicant,
    include_deleted: bool,
    out_json: *mut *mut c_char,
) -> SyncResult {
    guard(|| {
        if handle.is_null() || out_json.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant
            .handle
            .block_on(store.document_ids(include_deleted))
        {
            Ok(ids) => write_json(out_json, &ids),
            Err(error) => store_result(&error),
        }
    })
}

/// # Safety
/// Valid handle and out pointer.
#[no_mangle]
pub unsafe extern "C" fn replicant_count_documents(
    handle: *mut Replicant,
    out_count: *mut u64,
) -> SyncResult {
    guard(|| {
        if handle.is_null() || out_count.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant.handle.block_on(store.count_documents()) {
            Ok(count) => {
                *out_count = count;
                SyncResult::Success
            }
            Err(error) => store_result(&error),
        }
    })
}

/// Documents with changes the server has not acknowledged, except parked ones (`replicant_list_parked`).
///
/// # Safety
/// Valid handle and out pointer.
#[no_mangle]
pub unsafe extern "C" fn replicant_count_pending_sync(
    handle: *mut Replicant,
    out_count: *mut u64,
) -> SyncResult {
    guard(|| {
        if handle.is_null() || out_count.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant.handle.block_on(store.count_pending_sync()) {
            Ok(count) => {
                *out_count = count;
                SyncResult::Success
            }
            Err(error) => store_result(&error),
        }
    })
}

/// # Safety
/// `handle` must be valid or null.
#[no_mangle]
pub unsafe extern "C" fn replicant_is_connected(handle: *mut Replicant) -> bool {
    !handle.is_null() && (*handle).handle.state().connection == ConnectionView::Connected
}

/// Connection, sync phase and halt reason, for a status indicator. Set
/// `out_state->struct_size = sizeof(ReplicantState)` first.
///
/// # Safety
/// Valid handle and out pointer.
#[no_mangle]
pub unsafe extern "C" fn replicant_get_state(
    handle: *mut Replicant,
    out_state: *mut ReplicantState,
) -> SyncResult {
    guard(|| {
        if handle.is_null() || out_state.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        if (*out_state).struct_size as usize != std::mem::size_of::<ReplicantState>() {
            return SyncResult::ErrorInvalidInput;
        }
        *out_state = ReplicantState::from(&(*handle).handle.state());
        SyncResult::Success
    })
}

/// Leaves `Halted` or retries now. Safe to call repeatedly: the engine dials at most once a second.
///
/// # Safety
/// Valid handle.
#[no_mangle]
pub unsafe extern "C" fn replicant_reconnect(handle: *mut Replicant) -> SyncResult {
    guard(|| {
        if handle.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        (*handle).handle.reconnect();
        SyncResult::Success
    })
}

/// The data dir's user id (provisional until the first join).
///
/// # Safety
/// Valid handle and out pointer; free the result with `replicant_string_free`.
#[no_mangle]
pub unsafe extern "C" fn replicant_get_user_id(
    handle: *mut Replicant,
    out_user_id: *mut *mut c_char,
) -> SyncResult {
    guard(|| {
        if handle.is_null() || out_user_id.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant.handle.block_on(store.user_id()) {
            Ok(user_id) => match CString::new(user_id.to_string()) {
                Ok(text) => {
                    *out_user_id = text.into_raw();
                    SyncResult::Success
                }
                Err(_) => SyncResult::ErrorSerialization,
            },
            Err(error) => store_result(&error),
        }
    })
}

/// # Safety
/// Valid handle and C string (a JSON array of JSON paths, e.g. `["$.body"]`).
#[no_mangle]
pub unsafe extern "C" fn replicant_configure_search(
    handle: *mut Replicant,
    paths_json: *const c_char,
) -> SyncResult {
    guard(|| {
        if handle.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        let paths: Vec<String> = match json_arg(paths_json).map(serde_json::from_value) {
            Ok(Ok(paths)) => paths,
            Ok(Err(_)) => return SyncResult::ErrorSerialization,
            Err(result) => return result,
        };
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant.handle.block_on(store.configure_search(&paths)) {
            Ok(()) => SyncResult::Success,
            Err(error) => store_result(&error),
        }
    })
}

/// FTS5 query (`music`, `tun*`, `"a phrase"`, `a AND b`, `title:word`); `limit` 0 means 100. A
/// query FTS5 cannot parse is `ErrorInvalidInput`.
///
/// # Safety
/// Valid handle, C string and out pointer; free the result with `replicant_string_free`.
#[no_mangle]
pub unsafe extern "C" fn replicant_search_documents(
    handle: *mut Replicant,
    query: *const c_char,
    limit: u32,
    out_json: *mut *mut c_char,
) -> SyncResult {
    guard(|| {
        let (false, false, Some(query)) = (handle.is_null(), out_json.is_null(), str_arg(query))
        else {
            return SyncResult::ErrorInvalidInput;
        };
        let limit = if limit == 0 { 100 } else { limit };
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant
            .handle
            .block_on(store.search_documents(query, limit))
        {
            Ok(documents) => write_json(out_json, &documents),
            Err(error) => store_result(&error),
        }
    })
}

/// # Safety
/// Valid handle.
#[no_mangle]
pub unsafe extern "C" fn replicant_rebuild_search_index(handle: *mut Replicant) -> SyncResult {
    guard(|| {
        if handle.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant.handle.block_on(store.rebuild_search_index()) {
            Ok(()) => SyncResult::Success,
            Err(error) => store_result(&error),
        }
    })
}

/// `event_filter`: -1 every document event, 1 `DocumentChanged` only, 2 `DocumentDeleted` only.
/// The first registration on a handle fixes the thread that must call `replicant_process_events`.
///
/// # Safety
/// Valid handle; `context` must outlive the handle.
#[no_mangle]
pub unsafe extern "C" fn replicant_register_document_callback(
    handle: *mut Replicant,
    callback: DocumentEventCallback,
    context: *mut c_void,
    event_filter: i32,
) -> SyncResult {
    guard(|| {
        let (false, Some(callback)) = (handle.is_null(), callback) else {
            return SyncResult::ErrorInvalidInput;
        };
        let filter = match event_filter {
            -1 => None,
            1 => Some(EventType::DocumentChanged),
            2 => Some(EventType::DocumentDeleted),
            _ => return SyncResult::ErrorInvalidInput,
        };
        (*handle)
            .dispatcher
            .register_document(callback, context, filter);
        SyncResult::Success
    })
}

/// `SyncStarted`, `SyncCompleted`, `DatabaseChanged` and `IdentityAdopted`.
///
/// # Safety
/// Valid handle; `context` must outlive the handle.
#[no_mangle]
pub unsafe extern "C" fn replicant_register_sync_callback(
    handle: *mut Replicant,
    callback: SyncEventCallback,
    context: *mut c_void,
) -> SyncResult {
    guard(|| {
        let (false, Some(callback)) = (handle.is_null(), callback) else {
            return SyncResult::ErrorInvalidInput;
        };
        (*handle).dispatcher.register_sync(callback, context);
        SyncResult::Success
    })
}

/// # Safety
/// Valid handle; `context` must outlive the handle.
#[no_mangle]
pub unsafe extern "C" fn replicant_register_error_callback(
    handle: *mut Replicant,
    callback: ErrorEventCallback,
    context: *mut c_void,
) -> SyncResult {
    guard(|| {
        let (false, Some(callback)) = (handle.is_null(), callback) else {
            return SyncResult::ErrorInvalidInput;
        };
        (*handle).dispatcher.register_error(callback, context);
        SyncResult::Success
    })
}

/// # Safety
/// Valid handle; `context` must outlive the handle.
#[no_mangle]
pub unsafe extern "C" fn replicant_register_connection_callback(
    handle: *mut Replicant,
    callback: ConnectionEventCallback,
    context: *mut c_void,
) -> SyncResult {
    guard(|| {
        let (false, Some(callback)) = (handle.is_null(), callback) else {
            return SyncResult::ErrorInvalidInput;
        };
        (*handle).dispatcher.register_connection(callback, context);
        SyncResult::Success
    })
}

/// # Safety
/// Valid handle; `context` must outlive the handle.
#[no_mangle]
pub unsafe extern "C" fn replicant_register_conflict_callback(
    handle: *mut Replicant,
    callback: ConflictEventCallback,
    context: *mut c_void,
) -> SyncResult {
    guard(|| {
        let (false, Some(callback)) = (handle.is_null(), callback) else {
            return SyncResult::ErrorInvalidInput;
        };
        (*handle).dispatcher.register_conflict(callback, context);
        SyncResult::Success
    })
}

/// Runs the callbacks for every queued event. Must be called on the thread that registered
/// the callbacks: refused with `ErrorWrongThread` elsewhere and `ErrorNoCallbacks` before any
/// registration; nothing is lost either way. The thread cannot be changed later. A
/// `replicant_destroy` of this handle from inside a callback frees it when this call returns;
/// the rest of the batch is not delivered. A call from inside a callback returns
/// `ErrorInvalidInput`. No rebind: if the registering thread ends, register again on a new
/// handle.
///
/// # Safety
/// Valid handle; `out_processed_count` may be null.
#[no_mangle]
pub unsafe extern "C" fn replicant_process_events(
    handle: *mut Replicant,
    out_processed_count: *mut u32,
) -> SyncResult {
    if handle.is_null() {
        return SyncResult::ErrorInvalidInput;
    }
    let replicant = &*handle;
    if replicant
        .pump
        .compare_exchange(
            PUMP_IDLE,
            PUMP_DISPATCHING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_err()
    {
        return SyncResult::ErrorInvalidInput; // already pumping (a re-entrant call)
    }
    let result = guard(|| {
        let destroy_requested = || replicant.pump.load(Ordering::SeqCst) == PUMP_DESTROY_REQUESTED;
        match replicant
            .dispatcher
            .process(|| replicant.handle.take_events(), destroy_requested)
        {
            Ok(processed) => {
                if !out_processed_count.is_null() {
                    *out_processed_count = u32::try_from(processed).unwrap_or(u32::MAX);
                }
                SyncResult::Success
            }
            Err(DispatchError::WrongThread) => SyncResult::ErrorWrongThread,
            Err(DispatchError::NoCallbacks) => SyncResult::ErrorNoCallbacks,
        }
    });
    if replicant
        .pump
        .compare_exchange(
            PUMP_DISPATCHING,
            PUMP_IDLE,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_err()
    {
        free(handle);
    }
    result
}

/// # Safety
/// `s` must come from this library and not be freed twice.
#[no_mangle]
pub unsafe extern "C" fn replicant_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

#[no_mangle]
pub extern "C" fn replicant_get_version() -> *mut c_char {
    std::panic::catch_unwind(|| {
        CString::new(env!("CARGO_PKG_VERSION")).map_or(ptr::null_mut(), CString::into_raw)
    })
    .unwrap_or(ptr::null_mut())
}

// ===== Enrollment and stored credentials =====

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
    guard(|| {
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
    })
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
    guard(|| {
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
                    crate::enrollment::EnrollError::InvalidResponse => {
                        SyncResult::ErrorSerialization
                    }
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
    })
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
    guard(|| {
        if data_dir.is_null()
            || out_api_key.is_null()
            || out_secret.is_null()
            || out_user_id.is_null()
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
    })
}

/// Stores credentials in `data_dir` (encrypted at rest) and tells this process's engines on
/// that data dir. `email` (may be null; `""` counts as none) signs joins; `user_id` must be a
/// real UUID.
///
/// # Safety
/// Valid C strings; `email` may be null.
#[no_mangle]
pub unsafe extern "C" fn replicant_store_credentials(
    data_dir: *const c_char,
    email: *const c_char,
    api_key: *const c_char,
    secret: *const c_char,
    user_id: *const c_char,
) -> SyncResult {
    guard(|| {
        let (Some(data_dir), Some(api_key), Some(secret), Some(user_id), Ok(email)) = (
            str_arg(data_dir),
            str_arg(api_key),
            str_arg(secret),
            uuid_arg(user_id),
            nullable_str_arg(email),
        ) else {
            return SyncResult::ErrorInvalidInput;
        };
        if user_id.is_nil() {
            return SyncResult::ErrorInvalidInput;
        }
        let credentials = crate::secret_store::Credentials {
            api_key: api_key.to_string(),
            secret: secret.to_string(),
            user_id,
            email: email.filter(|email| !email.is_empty()).map(str::to_string),
        };
        match crate::secret_store::store(Path::new(data_dir), &credentials) {
            Ok(()) => {
                host::credentials_changed(Path::new(data_dir));
                SyncResult::Success
            }
            Err(_) => SyncResult::ErrorDatabase,
        }
    })
}

/// Removes the stored credentials (sign-out) and tells this process's engines on `data_dir`:
/// they halt as not enrolled and never join with the removed credentials. An engine in another
/// process ends its live connection within about a second and never joins with the removed
/// credentials.
///
/// # Safety
/// Valid C string.
#[no_mangle]
pub unsafe extern "C" fn replicant_clear_credentials(data_dir: *const c_char) -> SyncResult {
    guard(|| {
        let Some(data_dir) = str_arg(data_dir) else {
            return SyncResult::ErrorInvalidInput;
        };
        match crate::secret_store::clear(Path::new(data_dir)) {
            Ok(()) => {
                host::credentials_changed(Path::new(data_dir));
                SyncResult::Success
            }
            Err(_) => SyncResult::ErrorDatabase,
        }
    })
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
        // The calls run on their own thread, so a caller inside a runtime is fine. The
        // insecure URL makes them fail fast without any network traffic.
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

    #[test]
    fn state_names_every_halt_reason() {
        let halted = |reason| {
            ReplicantState::from(&EngineState {
                connection: ConnectionView::Halted(reason),
                sync: SyncView::Idle,
            })
            .halt_reason
        };
        assert_eq!(
            halted(HaltReason::NotEnrolled),
            ReplicantHaltReason::HaltNotEnrolled
        );
        assert_eq!(
            halted(HaltReason::AuthInvalid),
            ReplicantHaltReason::HaltAuthInvalid
        );
        assert_eq!(
            halted(HaltReason::UpdateRequired),
            ReplicantHaltReason::HaltUpdateRequired
        );
        assert_eq!(
            halted(HaltReason::AccountDisabled),
            ReplicantHaltReason::HaltAccountDisabled
        );
        assert_eq!(
            halted(HaltReason::Other("identity_drift".into())),
            ReplicantHaltReason::HaltIdentityDrift
        );
        assert_eq!(
            halted(HaltReason::Other("store_error".into())),
            ReplicantHaltReason::HaltOther
        );
        assert_eq!(
            ReplicantState::from(&EngineState {
                connection: ConnectionView::Connected,
                sync: SyncView::Live,
            }),
            ReplicantState {
                struct_size: std::mem::size_of::<ReplicantState>() as u32,
                connection: ReplicantConnection::ConnectionConnected,
                sync: ReplicantSync::SyncLive,
                halt_reason: ReplicantHaltReason::HaltNone,
            }
        );
    }

    #[test]
    fn a_panic_inside_an_entry_point_becomes_error_unknown() {
        assert_eq!(guard(|| panic!("boom")), SyncResult::ErrorUnknown);
    }
}
