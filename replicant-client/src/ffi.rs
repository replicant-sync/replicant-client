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

use crate::engine::doc::FieldConflict;
use crate::engine::list_merge::{ListMergeConfig, ListMergePolicy, PathPattern};
use crate::engine::machine::{ConnectionView, EngineState, HaltReason, SyncView};
use crate::events::{
    DispatchError, Dispatcher, EventType, ReplicantConflictEventCallback,
    ReplicantConnectionEventCallback, ReplicantDocumentEventCallback, ReplicantErrorEventCallback,
    ReplicantSyncEventCallback,
};
use crate::host::{self, Handle, HostConfig, OpenError};
use crate::store::StoreError;

/// Opaque handle. Every call except `replicant_process_events`, the
/// `replicant_register_*_callback` calls and the destroy calls is thread-safe; the first two run
/// only on the thread the first registration bound, and a destroy must not overlap any other
/// call on the handle (see `replicant_destroy`). Never call from an audio thread: every read and
/// write is a SQLite transaction.
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

/// The C ABI version is MAJOR.MINOR. A breaking change bumps the major; an addition (a function,
/// a struct field at the end, an enum value) bumps the minor. A host needs the library's major
/// equal to the header's, and its minor at least the minor that added what the host uses.
/// Hosts treat unknown enum values as unknown: an unknown `ReplicantEventType` is ignored, an
/// unknown `ReplicantHaltReason` is `Other`, an unknown error code is 0 (`Unknown`).
pub const REPLICANT_ABI_VERSION_MAJOR: u32 = 1;
/// See `REPLICANT_ABI_VERSION_MAJOR`.
pub const REPLICANT_ABI_VERSION_MINOR: u32 = 0;

/// The library's ABI version, packed: `(major << 16) | minor`.
#[no_mangle]
pub extern "C" fn replicant_abi_version() -> u32 {
    (REPLICANT_ABI_VERSION_MAJOR << 16) | REPLICANT_ABI_VERSION_MINOR
}

/// Longest email, in bytes.
pub const REPLICANT_EMAIL_MAX_LEN: usize = 254;
/// A user id's length; its buffer needs one more for the NUL.
pub const REPLICANT_USER_ID_LEN: usize = 36;
/// A document id's length; its buffer needs one more for the NUL.
pub const REPLICANT_DOCUMENT_ID_LEN: usize = 36;

/// Every entry point's body runs in here: a panic becomes `ErrorUnknown` instead of unwinding
/// into C, which would abort the host (a DAW).
fn guard(body: impl FnOnce() -> SyncResult) -> SyncResult {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or_else(|_| {
        tracing::error!("a replicant call panicked");
        SyncResult::ErrorUnknown
    })
}

/// cbindgen:prefix-with-name
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncResult {
    Success = 0,
    ErrorInvalidInput = -1,
    /// Enrollment only: the server could not be reached, or answered with an unexpected status.
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
    /// Migrating a v1 library failed. Show "Your library needs attention"; never fall back to a
    /// temporary library. Either the backup itself failed: no backup was written and the
    /// database is unchanged. Or the migration failed after the backup: the documents are
    /// unchanged but 0.6 builds cannot open the database; restoring the backup
    /// (`<database>.v1-backup`, or `<database>.v1-backup-<unix seconds>` when that name was
    /// taken) is the way back.
    ErrorMigrationFailed = -10,
    /// Another process kept the database locked; try `replicant_create` again shortly.
    ErrorBusy = -11,
    /// `replicant_process_events` or a `replicant_register_*_callback` call came from another
    /// thread than the one the handle's first registration bound.
    ErrorWrongThread = -12,
    /// `replicant_process_events` was called before any callback was registered.
    ErrorNoCallbacks = -13,
    /// The kept copy's document was deleted: `replicant_restore_document` re-creates the copy.
    ErrorDocumentGone = -14,
    /// An output buffer is smaller than the value plus its NUL; see the `REPLICANT_*_LEN` limits.
    ErrorBufferTooSmall = -15,
    /// The server refused the enrollment code (wrong or expired): ask for a new one.
    ErrorTokenRejected = -16,
    ErrorUnknown = -99,
}

/// Every string is UTF-8 and copied by `replicant_create`.
#[repr(C)]
pub struct ReplicantConfig {
    /// `sizeof(ReplicantConfig)`. Smaller than the ABI 1.0 struct is refused
    /// (`ErrorInvalidInput`); the library reads only the fields it knows.
    pub struct_size: u32,
    /// Holds the database file and the stored credentials.
    pub data_dir: *const c_char,
    /// File name inside `data_dir`, e.g. "tonaldb.sqlite3"; a path separator or `..` is refused.
    pub database_file: *const c_char,
    pub server_url: *const c_char,
    /// May be null (`""` counts as null). Signs joins when the stored credentials carry no email.
    /// Credentials stored by 0.6 carry none: with a null email here the engine reports
    /// `NotEnrolled`, so a host upgrading from 0.6 must pass the user's email. Not part of the
    /// shared-engine check: a later handle on an open data dir never gets `ErrorConfigMismatch`
    /// for a different email, and the email of the handle that started the engine is the one used.
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

/// Merge policy for a list changed on both sides. `ReplicantListMerge_Full` is refused for now.
/// cbindgen:prefix-with-name
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicantListMerge {
    /// Element by element while positions line up on both sides; otherwise the server's list is
    /// kept and the local one set aside.
    Append = 0,
    /// Any change on both sides keeps the server's list and sets the local one aside.
    Atomic = 1,
    Full = 2,
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

/// cbindgen:prefix-with-name
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicantConnection {
    /// Never reported: `replicant_create` returns after the engine left it.
    Idle = 0,
    Disconnected = 1,
    Connecting = 2,
    Connected = 3,
    /// Not retrying on its own; see `halt_reason`.
    Halted = 4,
    Stopped = 5,
}

/// cbindgen:prefix-with-name
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicantSync {
    Idle = 0,
    CatchingUp = 1,
    Live = 2,
}

/// cbindgen:prefix-with-name
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicantHaltReason {
    None = 0,
    /// No stored credentials: sign in.
    NotEnrolled = 1,
    /// The server refused the credentials: sign in again.
    AuthInvalid = 2,
    /// The server needs a newer client.
    UpdateRequired = 3,
    /// The server disabled the account. A different account signed in on this data dir, in any
    /// process, is picked up within about 3 s; the same account stays halted.
    AccountDisabled = 4,
    /// This data dir belongs to another account.
    IdentityDrift = 5,
    Other = 6,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicantState {
    /// Set to `sizeof(ReplicantState)` before `replicant_get_state`; smaller than the ABI 1.0
    /// struct is refused. The library writes only the fields it knows and sets this to their
    /// size, so a host from a later version can tell which of its fields were filled.
    pub struct_size: u32,
    pub connection: ReplicantConnection,
    pub sync: ReplicantSync,
    pub halt_reason: ReplicantHaltReason,
}

/// The ABI 1.0 struct sizes: later versions append fields and keep accepting these.
// A field added after 1.0 is read through a raw pointer, and only when `struct_size` covers it.
const CONFIG_SIZE_V1_0: usize = std::mem::offset_of!(ReplicantConfig, list_merge_rules_json)
    + std::mem::size_of::<*const c_char>();
const STATE_SIZE_V1_0: usize =
    std::mem::offset_of!(ReplicantState, halt_reason) + std::mem::size_of::<ReplicantHaltReason>();

impl From<&EngineState> for ReplicantState {
    fn from(state: &EngineState) -> Self {
        let (connection, halt_reason) = match &state.connection {
            ConnectionView::Idle => (ReplicantConnection::Idle, ReplicantHaltReason::None),
            ConnectionView::Disconnected => {
                (ReplicantConnection::Disconnected, ReplicantHaltReason::None)
            }
            ConnectionView::Connecting => {
                (ReplicantConnection::Connecting, ReplicantHaltReason::None)
            }
            ConnectionView::Connected => {
                (ReplicantConnection::Connected, ReplicantHaltReason::None)
            }
            ConnectionView::Stopped => (ReplicantConnection::Stopped, ReplicantHaltReason::None),
            ConnectionView::Halted(reason) => (
                ReplicantConnection::Halted,
                match reason {
                    HaltReason::NotEnrolled => ReplicantHaltReason::NotEnrolled,
                    HaltReason::AuthInvalid => ReplicantHaltReason::AuthInvalid,
                    HaltReason::UpdateRequired => ReplicantHaltReason::UpdateRequired,
                    HaltReason::AccountDisabled => ReplicantHaltReason::AccountDisabled,
                    HaltReason::Other(code) if code == "identity_drift" => {
                        ReplicantHaltReason::IdentityDrift
                    }
                    HaltReason::Other(_) => ReplicantHaltReason::Other,
                },
            ),
        };
        let sync = match state.sync {
            SyncView::Idle => ReplicantSync::Idle,
            SyncView::CatchingUp => ReplicantSync::CatchingUp,
            SyncView::Live => ReplicantSync::Live,
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
        StoreError::NotFound(_) | StoreError::NoKeptCopy(_) => SyncResult::ErrorNotFound,
        StoreError::NoFieldConflict(_) => SyncResult::ErrorInvalidInput,
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

/// Nulls a caller's out pointer first, so it is null after any failure.
unsafe fn clear_out(out: *mut *mut c_char) {
    if !out.is_null() {
        *out = ptr::null_mut();
    }
}

fn registered(result: Result<(), DispatchError>) -> SyncResult {
    match result {
        Ok(()) => SyncResult::Success,
        Err(_) => SyncResult::ErrorWrongThread,
    }
}

fn dispatch_result(error: DispatchError) -> SyncResult {
    match error {
        DispatchError::WrongThread => SyncResult::ErrorWrongThread,
        DispatchError::NoCallbacks => SyncResult::ErrorNoCallbacks,
    }
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
/// network. On success `*out_handle` is set; on any failure, a null `config` included, it is null.
///
/// The stored credentials are read before this returns, so `replicant_get_state` is already
/// true: `Halted`/`NotEnrolled` without credentials, else dialling (or, for a later handle, the
/// shared engine's current state). An engine that starts without credentials also sends one
/// fatal `not_enrolled` error, to the handle that started it; a later handle on that engine is
/// not sent it again and reads `replicant_get_state` instead.
///
/// # Safety
/// `config` and `out_handle` must be valid; the config's strings valid C strings (`email` may be null).
#[no_mangle]
pub unsafe extern "C" fn replicant_create(
    config: *const ReplicantConfig,
    out_handle: *mut *mut Replicant,
) -> SyncResult {
    if !out_handle.is_null() {
        *out_handle = ptr::null_mut();
    }
    guard(|| {
        if config.is_null() || out_handle.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        let config = &*config;
        if (config.struct_size as usize) < CONFIG_SIZE_V1_0 {
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
        let (Some(list_merge), Ok(fallback_email)) =
            (list_merge_arg(config), nullable_str_arg(config.email))
        else {
            return SyncResult::ErrorInvalidInput;
        };
        let host_config = HostConfig {
            data_dir: PathBuf::from(data_dir),
            database_file: database_file.to_string(),
            server_url: server_url.to_string(),
            fallback_email: fallback_email
                .filter(|email| !email.is_empty())
                .map(str::to_string),
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
/// engine stops on a Replicant thread afterwards (never waiting on the network).
///
/// A destroy must not overlap any other call on this handle, on any thread: the host ends every
/// other use of the handle first. The one exception is a destroy from inside one of this
/// handle's callbacks, which is deferred: the free happens when `replicant_process_events`
/// returns. Other handles, including those on the same data dir, are unaffected.
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
/// unloading safe (see `replicant_destroy`). The same overlap rule applies. Called from inside
/// one of this handle's callbacks it cannot wait: it returns false at once and the handle is
/// freed when `replicant_process_events` returns.
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
/// `out_document_id` (`REPLICANT_DOCUMENT_ID_LEN + 1` bytes).
///
/// # Safety
/// Valid handle, C string, and a `REPLICANT_DOCUMENT_ID_LEN + 1`-byte buffer.
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

/// The document as a JSON object. `ErrorNotFound` when missing or deleted.
///
/// JSON naming in this API: a document's own id is `id`; any other record that refers to a
/// document names it `doc_id`. Ids are lowercase hyphenated UUID strings. A document object has:
/// - `id` (string): the document's id.
/// - `owner_id` (string or null): the owning user; null only for a legacy document with none.
/// - `author_id` (string or null): the author the server reports; null until it has synced.
/// - `title` (string or null): the content's `title` string, cut to 128 characters, else
///   the title the server sent.
/// - `content`: the host's own JSON value, as last written or synced.
/// - `read_only` (bool): the server marked it read-only (a publication); writes are refused.
/// - `visibility` (string): `public` (curated or read-only) or `private`.
/// - `source_doc_id`, `derived_from` (string or null): ids of the documents it came from.
/// - `created_at`, `updated_at` (string): RFC 3339 in UTC, e.g.
///   `2026-10-01T07:51:25.301096+00:00`. Times on this device: when the document was first
///   stored here, and when its content last changed here (a local edit or a synced change).
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
        clear_out(out_json);
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

/// Every visible document as a JSON array of document objects (see `replicant_get_document`).
///
/// # Safety
/// Valid handle and out pointer; free the result with `replicant_string_free`.
#[no_mangle]
pub unsafe extern "C" fn replicant_get_all_documents(
    handle: *mut Replicant,
    out_json: *mut *mut c_char,
) -> SyncResult {
    guard(|| {
        clear_out(out_json);
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

/// Document ids as a JSON array of strings; `include_deleted` adds documents whose delete is not
/// yet sent.
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
        clear_out(out_json);
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

/// Documents with changes the server has not acknowledged, except parked ones
/// (`replicant_list_parked`).
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

/// Whether the engine is connected now. `replicant_get_state` says more.
///
/// # Safety
/// `handle` must be valid or null.
#[no_mangle]
pub unsafe extern "C" fn replicant_is_connected(handle: *mut Replicant) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        !handle.is_null() && (*handle).handle.state().connection == ConnectionView::Connected
    }))
    .unwrap_or(false)
}

/// Connection, sync phase and halt reason, for a status indicator. Set
/// `out_state->struct_size = sizeof(ReplicantState)` first. True from the moment
/// `replicant_create` returns: with no stored credentials it is already `Halted`/`NotEnrolled`.
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
        // Raw field writes: the host's struct may hold values that are not valid Rust enums.
        let host_size = ptr::addr_of!((*out_state).struct_size).read() as usize;
        if host_size < STATE_SIZE_V1_0 {
            return SyncResult::ErrorInvalidInput;
        }
        let state = ReplicantState::from(&(*handle).handle.state());
        ptr::addr_of_mut!((*out_state).connection).write(state.connection);
        ptr::addr_of_mut!((*out_state).sync).write(state.sync);
        ptr::addr_of_mut!((*out_state).halt_reason).write(state.halt_reason);
        ptr::addr_of_mut!((*out_state).struct_size).write(STATE_SIZE_V1_0 as u32);
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
        clear_out(out_user_id);
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
/// query FTS5 cannot parse is `ErrorInvalidInput`. The result is a JSON array of document
/// objects (see `replicant_get_document`), best match first.
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
        clear_out(out_json);
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

/// One kept copy as the host lists it.
#[derive(Serialize)]
struct KeptCopyJson<'a> {
    recovered_id: i64,
    doc_id: Uuid,
    title: Option<&'a str>,
    reason: &'a str,
    recovered_at: i64,
    content: &'a Value,
    fields: &'a Option<Vec<FieldConflict>>,
}

/// Kept copies (local content sync set aside), newest first, as a JSON array of objects:
/// - `recovered_id` (integer): the copy's id, for `replicant_dismiss_recovered` and the restores.
/// - `doc_id` (string): the document the copy was kept from (it may since have been deleted).
/// - `title` (string or null): the kept content's `title`, when it is a string.
/// - `reason` (string): `conflict`, `field_conflict`, `delete_wins`, `delete_superseded`,
///   `delete_refused`, `delete_publication`, `became_publication`, `create_rejected` or
///   `unmigratable` (set aside while upgrading the database).
/// - `recovered_at` (integer): when it was kept, in Unix seconds.
/// - `content`: the host's own JSON value, as it was locally when kept.
/// - `fields` (array or null): null for a whole-document copy; else one object per conflicting
///   path: `path` (string, a JSON Pointer), `local_value` (any JSON; null when removed locally),
///   `local_removed` (bool: the local side removed the path).
///
/// Copies never expire: they stay until dismissed or restored. A copy that cannot be read is left
/// out (and logged).
///
/// # Safety
/// Valid handle and out pointer; free the result with `replicant_string_free`.
#[no_mangle]
pub unsafe extern "C" fn replicant_list_recovered(
    handle: *mut Replicant,
    out_json: *mut *mut c_char,
) -> SyncResult {
    guard(|| {
        clear_out(out_json);
        if handle.is_null() || out_json.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant.handle.block_on(store.list_recovered()) {
            Ok(copies) => {
                let listed: Vec<KeptCopyJson> = copies
                    .iter()
                    .map(|copy| KeptCopyJson {
                        recovered_id: copy.id,
                        doc_id: copy.doc_id,
                        title: copy.content.get("title").and_then(Value::as_str),
                        reason: &copy.reason,
                        recovered_at: copy.recovered_at,
                        content: &copy.content,
                        fields: &copy.fields,
                    })
                    .collect();
                write_json(out_json, &listed)
            }
            Err(error) => store_result(&error),
        }
    })
}

/// Deletes a kept copy for good; `ErrorNotFound` when it is already gone.
///
/// # Safety
/// Valid handle.
#[no_mangle]
pub unsafe extern "C" fn replicant_dismiss_recovered(
    handle: *mut Replicant,
    recovered_id: i64,
) -> SyncResult {
    guard(|| {
        if handle.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant
            .handle
            .block_on(store.dismiss_recovered(recovered_id))
        {
            Ok(true) => SyncResult::Success,
            Ok(false) => SyncResult::ErrorNotFound,
            Err(error) => store_result(&error),
        }
    })
}

/// Re-creates a kept copy's full content as a new document, with a new id written to
/// `out_document_id` (`REPLICANT_DOCUMENT_ID_LEN + 1` bytes), and removes the copy. Works for
/// any copy, a field copy too (the way out when its document is gone). `ErrorNotFound` when the
/// copy is gone (another process may have dismissed or restored it).
///
/// # Safety
/// Valid handle and a `REPLICANT_DOCUMENT_ID_LEN + 1`-byte buffer.
#[no_mangle]
pub unsafe extern "C" fn replicant_restore_document(
    handle: *mut Replicant,
    recovered_id: i64,
    out_document_id: *mut c_char,
) -> SyncResult {
    guard(|| {
        if handle.is_null() || out_document_id.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant
            .handle
            .block_on(store.restore_document(recovered_id))
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

/// Writes a field copy's kept values back at their paths as a local edit (every other field
/// keeps its current value) and removes the copy.
///
/// A list conflict is kept as the whole list, so restoring it puts that list back exactly.
/// `ErrorNotFound`: the copy is gone. `ErrorInvalidInput`: it is a whole-document copy (its
/// `fields` is null); it stays, and `replicant_restore_document` restores it.
/// `ErrorDocumentGone`: its document was deleted, and `ErrorNotWritable`: it became read-only;
/// the copy stays in both cases, and `replicant_restore_document` brings it back as a new
/// document.
///
/// # Safety
/// Valid handle.
#[no_mangle]
pub unsafe extern "C" fn replicant_restore_fields(
    handle: *mut Replicant,
    recovered_id: i64,
) -> SyncResult {
    guard(|| {
        if handle.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant
            .handle
            .block_on(store.restore_fields(recovered_id))
        {
            Ok(()) => {
                replicant.handle.notify_outbox();
                SyncResult::Success
            }
            Err(error) => store_result(&error),
        }
    })
}

/// Documents that stopped uploading until their next local edit, as a JSON array of objects:
/// `doc_id` (string, the parked document) and `code` (string: `validation`, `forbidden`,
/// `too_large` or `diverged`). Parked documents are not counted in
/// `replicant_count_pending_sync`; a new local edit un-parks one.
///
/// # Safety
/// Valid handle and out pointer; free the result with `replicant_string_free`.
#[no_mangle]
pub unsafe extern "C" fn replicant_list_parked(
    handle: *mut Replicant,
    out_json: *mut *mut c_char,
) -> SyncResult {
    guard(|| {
        clear_out(out_json);
        if handle.is_null() || out_json.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        let replicant = &*handle;
        let store = replicant.handle.store();
        match replicant.handle.block_on(store.list_parked()) {
            Ok(parked) => write_json(out_json, &parked),
            Err(error) => store_result(&error),
        }
    })
}

/// `event_filter`: -1 every document event, 1 `DocumentChanged` only, 2 `DocumentDeleted` only.
/// One callback per kind: registering again replaces the previous callback, filter and context;
/// a null callback removes it (its events are then dropped when pumped). Once this returns, the
/// old context is never called again, even for the rest of a batch being pumped. The first
/// registration on a handle, of any kind and even a null one, binds its thread: later
/// registrations and `replicant_process_events` must come from that thread
/// (`ErrorWrongThread` otherwise).
///
/// # Safety
/// Valid handle; `context` must outlive the handle.
#[no_mangle]
pub unsafe extern "C" fn replicant_register_document_callback(
    handle: *mut Replicant,
    callback: ReplicantDocumentEventCallback,
    context: *mut c_void,
    event_filter: i32,
) -> SyncResult {
    guard(|| {
        if handle.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        let filter = match event_filter {
            -1 => None,
            1 => Some(EventType::DocumentChanged),
            2 => Some(EventType::DocumentDeleted),
            _ => return SyncResult::ErrorInvalidInput,
        };
        registered(
            (*handle)
                .dispatcher
                .register_document(callback, context, filter),
        )
    })
}

/// `SyncStarted`, `SyncCompleted`, `DatabaseChanged` and `IdentityAdopted`. Replaces or removes
/// as `replicant_register_document_callback` does.
///
/// # Safety
/// Valid handle; `context` must outlive the handle.
#[no_mangle]
pub unsafe extern "C" fn replicant_register_sync_callback(
    handle: *mut Replicant,
    callback: ReplicantSyncEventCallback,
    context: *mut c_void,
) -> SyncResult {
    guard(|| {
        if handle.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        registered((*handle).dispatcher.register_sync(callback, context))
    })
}

/// Replaces or removes as `replicant_register_document_callback` does.
///
/// # Safety
/// Valid handle; `context` must outlive the handle.
#[no_mangle]
pub unsafe extern "C" fn replicant_register_error_callback(
    handle: *mut Replicant,
    callback: ReplicantErrorEventCallback,
    context: *mut c_void,
) -> SyncResult {
    guard(|| {
        if handle.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        registered((*handle).dispatcher.register_error(callback, context))
    })
}

/// Replaces or removes as `replicant_register_document_callback` does.
///
/// # Safety
/// Valid handle; `context` must outlive the handle.
#[no_mangle]
pub unsafe extern "C" fn replicant_register_connection_callback(
    handle: *mut Replicant,
    callback: ReplicantConnectionEventCallback,
    context: *mut c_void,
) -> SyncResult {
    guard(|| {
        if handle.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        registered((*handle).dispatcher.register_connection(callback, context))
    })
}

/// Replaces or removes as `replicant_register_document_callback` does.
///
/// # Safety
/// Valid handle; `context` must outlive the handle.
#[no_mangle]
pub unsafe extern "C" fn replicant_register_conflict_callback(
    handle: *mut Replicant,
    callback: ReplicantConflictEventCallback,
    context: *mut c_void,
) -> SyncResult {
    guard(|| {
        if handle.is_null() {
            return SyncResult::ErrorInvalidInput;
        }
        registered((*handle).dispatcher.register_conflict(callback, context))
    })
}

/// Runs the callbacks for every queued event. Must be called on the thread of the handle's first
/// registration (a null one binds too): refused with `ErrorWrongThread` elsewhere, without
/// disturbing a pump running on the bound thread, and `ErrorNoCallbacks` before any
/// registration; nothing is lost either way. The thread cannot be changed later. A
/// `replicant_destroy` of this handle from inside a callback frees it when this call returns;
/// the rest of the batch is not delivered. A call from inside a callback returns
/// `ErrorInvalidInput`. No rebind: if the registering thread ends, register again on a new
/// handle.
///
/// Callbacks must not throw or unwind. Inside a callback every call is allowed except
/// `replicant_process_events` on the same handle; a `replicant_destroy` of it is deferred.
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
    // Checked before the pump flag, so a call from another thread never holds it.
    if let Err(error) = replicant.dispatcher.may_process() {
        return dispatch_result(error);
    }
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
            Err(error) => dispatch_result(error),
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

/// Found by `scripts/check_replicant_versions.py` in every binary that links this library.
#[used]
static VERSION_MARKER: &str = concat!("replicant-client-version=", env!("CARGO_PKG_VERSION"), "\0");

/// This library's version, e.g. "0.7.0". Static: never free it.
#[no_mangle]
pub extern "C" fn replicant_get_version() -> *const c_char {
    VERSION_MARKER["replicant-client-version=".len()..]
        .as_ptr()
        .cast()
}

// ===== Enrollment and sign-out =====

/// Asks the server to email an enrollment code to `email`. Needs no handle. Results:
/// - `Success`: the server accepted the request (HTTP 202).
/// - `ErrorInvalidInput`: a null or non-UTF-8 argument, an empty or over-long email, or a
///   `base_url` that is not https (`http://localhost` and `http://127.0.0.1` excepted). The
///   server is not contacted.
/// - `ErrorConnection`: the server could not be reached, timed out, or answered with any status
///   other than 202: a 4xx (429 when rate limited) or a 5xx. Retry later.
/// - `ErrorUnknown`: the library could not start the request.
///
/// Blocks the calling thread for the HTTP round trip (up to about 10 s to connect and 30 s for
/// the request); never call it from an audio or UI thread.
///
/// # Safety
/// `base_url` and `email` must be valid, non-null C strings.
#[no_mangle]
pub unsafe extern "C" fn replicant_enroll_request(
    base_url: *const c_char,
    email: *const c_char,
) -> SyncResult {
    guard(|| {
        let (Some(base_url), Some(email)) = (str_arg(base_url), str_arg(email)) else {
            return SyncResult::ErrorInvalidInput;
        };
        if email.is_empty() || email.len() > REPLICANT_EMAIL_MAX_LEN {
            return SyncResult::ErrorInvalidInput;
        }
        let (base_url, email) = (base_url.to_string(), email.to_string());
        let join_result = std::thread::spawn(move || {
            let runtime = Runtime::new().map_err(|_| SyncResult::ErrorUnknown)?;
            runtime
                .block_on(crate::enrollment::request(&base_url, &email))
                .map_err(|error| match error {
                    crate::enrollment::EnrollError::InsecureUrl => SyncResult::ErrorInvalidInput,
                    _ => SyncResult::ErrorConnection,
                })
        })
        .join();
        match join_result {
            Ok(Ok(())) => SyncResult::Success,
            Ok(Err(result)) => result,
            Err(_) => SyncResult::ErrorUnknown,
        }
    })
}

/// Exchanges an enrollment code for credentials and stores them in `data_dir` (encrypted at rest)
/// with `email`; the api key and secret never leave the library. Writes the user id into
/// `out_user_id` (`user_id_cap` bytes, at least `REPLICANT_USER_ID_LEN + 1`).
/// Results:
/// - `Success`: stored; this process's engines on `data_dir` sign in at once, and engines in
///   other processes within about 3 s.
/// - `ErrorInvalidInput`: a null or non-UTF-8 argument, an empty or over-long email, or a
///   `base_url` that is not https (localhost excepted). The server is not contacted.
/// - `ErrorBufferTooSmall`: `user_id_cap` is too small. The server is not contacted.
/// - `ErrorDatabase`: `data_dir` cannot hold credentials. This is checked before the server is
///   contacted, so the code stays valid; if storing still fails after the claim, the code was
///   used: request a new one.
/// - `ErrorTokenRejected`: the server refused the code (wrong or expired).
/// - `ErrorConnection`: the server could not be reached, timed out, or answered with any status
///   other than 200 or 401 (429 when rate limited). Retry later.
/// - `ErrorSerialization`: the server's reply was not valid credentials.
///
/// Blocks the calling thread for the HTTP round trip (up to about 10 s to connect and 30 s for
/// the request); never call it from an audio or UI thread.
///
/// # Safety
/// All string pointers must be valid, non-null C strings; `out_user_id` must reference a
/// writable buffer of at least `user_id_cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn replicant_enroll_claim(
    base_url: *const c_char,
    data_dir: *const c_char,
    email: *const c_char,
    token: *const c_char,
    out_user_id: *mut c_char,
    user_id_cap: usize,
) -> SyncResult {
    guard(|| {
        let (Some(base_url), Some(data_dir), Some(email), Some(token), false) = (
            str_arg(base_url),
            str_arg(data_dir),
            str_arg(email),
            str_arg(token),
            out_user_id.is_null(),
        ) else {
            return SyncResult::ErrorInvalidInput;
        };
        if email.is_empty() || email.len() > REPLICANT_EMAIL_MAX_LEN {
            return SyncResult::ErrorInvalidInput;
        }
        if user_id_cap < REPLICANT_USER_ID_LEN + 1 {
            return SyncResult::ErrorBufferTooSmall;
        }
        if crate::secret_store::prepare(Path::new(data_dir)).is_err() {
            return SyncResult::ErrorDatabase;
        }
        let (base_url, request_email, token) =
            (base_url.to_string(), email.to_string(), token.to_string());
        let join_result = std::thread::spawn(move || {
            let runtime = Runtime::new().map_err(|_| SyncResult::ErrorUnknown)?;
            runtime
                .block_on(crate::enrollment::claim(&base_url, &request_email, &token))
                .map_err(|error| match error {
                    crate::enrollment::EnrollError::InvalidToken => SyncResult::ErrorTokenRejected,
                    crate::enrollment::EnrollError::InsecureUrl => SyncResult::ErrorInvalidInput,
                    crate::enrollment::EnrollError::InvalidResponse => {
                        SyncResult::ErrorSerialization
                    }
                    crate::enrollment::EnrollError::Http(_) => SyncResult::ErrorConnection,
                })
        })
        .join();
        let mut credentials = match join_result {
            Ok(Ok(credentials)) => credentials,
            Ok(Err(result)) => return result,
            Err(_) => return SyncResult::ErrorUnknown,
        };
        credentials.email = Some(email.to_string());
        match crate::secret_store::store(Path::new(data_dir), &credentials) {
            Ok(()) => {
                host::credentials_changed(Path::new(data_dir));
                write_id(out_user_id, credentials.user_id);
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

    #[tokio::test]
    async fn enroll_ffi_is_callable_from_within_a_runtime() {
        // The calls run on their own thread, so a caller inside a runtime is fine. The
        // insecure URL makes them fail fast without any network traffic.
        let url = CString::new("http://example.com").unwrap();
        let email = CString::new("rt@test.com").unwrap();
        let result = unsafe { replicant_enroll_request(url.as_ptr(), email.as_ptr()) };
        assert_ne!(result, SyncResult::Success);

        let token = CString::new("TOK").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let data_dir = CString::new(dir.path().to_str().unwrap()).unwrap();
        let mut uid = [0i8; 37];
        let result = unsafe {
            replicant_enroll_claim(
                url.as_ptr(),
                data_dir.as_ptr(),
                email.as_ptr(),
                token.as_ptr(),
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
            ReplicantHaltReason::NotEnrolled
        );
        assert_eq!(
            halted(HaltReason::AuthInvalid),
            ReplicantHaltReason::AuthInvalid
        );
        assert_eq!(
            halted(HaltReason::UpdateRequired),
            ReplicantHaltReason::UpdateRequired
        );
        assert_eq!(
            halted(HaltReason::AccountDisabled),
            ReplicantHaltReason::AccountDisabled
        );
        assert_eq!(
            halted(HaltReason::Other("identity_drift".into())),
            ReplicantHaltReason::IdentityDrift
        );
        assert_eq!(
            halted(HaltReason::Other("store_error".into())),
            ReplicantHaltReason::Other
        );
        assert_eq!(
            ReplicantState::from(&EngineState {
                connection: ConnectionView::Connected,
                sync: SyncView::Live,
            }),
            ReplicantState {
                struct_size: std::mem::size_of::<ReplicantState>() as u32,
                connection: ReplicantConnection::Connected,
                sync: ReplicantSync::Live,
                halt_reason: ReplicantHaltReason::None,
            }
        );
    }

    #[test]
    fn this_version_s_structs_are_the_abi_1_0_sizes() {
        assert_eq!(std::mem::size_of::<ReplicantConfig>(), CONFIG_SIZE_V1_0);
        assert_eq!(std::mem::size_of::<ReplicantState>(), STATE_SIZE_V1_0);
    }

    #[test]
    fn a_panic_inside_an_entry_point_becomes_error_unknown() {
        assert_eq!(guard(|| panic!("boom")), SyncResult::ErrorUnknown);
    }

    #[test]
    fn the_version_marker_names_this_crate_version() {
        assert_eq!(
            VERSION_MARKER,
            concat!("replicant-client-version=", env!("CARGO_PKG_VERSION"), "\0")
        );
        assert_eq!(
            unsafe { CStr::from_ptr(replicant_get_version()) }
                .to_str()
                .unwrap(),
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn a_restored_document_is_uploaded_without_another_write() {
        use crate::driver::test_server::ScriptedServer;
        use crate::secret_store::{self, Credentials};
        use crate::store::test_support::{exec, ME};

        let server_runtime = Runtime::new().unwrap();
        let server = server_runtime.block_on(ScriptedServer::start(ME));
        let dir = tempfile::tempdir().unwrap();
        secret_store::store(
            dir.path(),
            &Credentials {
                api_key: "k1".into(),
                secret: "rps_test".into(),
                user_id: ME,
                email: None,
            },
        )
        .unwrap();
        let strings = [
            dir.path().to_str().unwrap(),
            "replicant.sqlite3",
            server.url.as_str(),
            "a@b.c",
            "Test Host",
            "1.0",
        ]
        .map(|text| CString::new(text).unwrap());
        let config = ReplicantConfig {
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
        let mut handle = ptr::null_mut();
        assert_eq!(
            unsafe { replicant_create(&config, &mut handle) },
            SyncResult::Success
        );
        let replicant = unsafe { &*handle };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while replicant.handle.state().sync != SyncView::Live {
            assert!(
                std::time::Instant::now() < deadline,
                "engine never went live"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        replicant.handle.block_on(exec(
            &replicant.handle.store(),
            "INSERT INTO recovered (doc_id, content, reason, recovered_at) \
             VALUES ('00000000-0000-0000-0000-000000000001', '{\"title\":\"Kept\"}', 'delete_wins', 100)",
        ));
        let recovered_id = replicant
            .handle
            .block_on(replicant.handle.store().list_recovered())
            .unwrap()[0]
            .id;
        let mut restored = [0 as c_char; 37];
        assert_eq!(
            unsafe { replicant_restore_document(handle, recovered_id, restored.as_mut_ptr()) },
            SyncResult::Success
        );
        let doc_id: Uuid = unsafe { CStr::from_ptr(restored.as_ptr()) }
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while server.uploads_for(doc_id).is_empty() {
            assert!(std::time::Instant::now() < deadline, "never uploaded");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(unsafe { replicant_destroy_and_wait(handle, 10_000) });
    }
}
