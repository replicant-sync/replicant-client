//! Host callbacks. The first registration fixes the thread that must run them; events wait in
//! the handle's queue until `replicant_process_events` is called on that thread.

use std::ffi::{c_char, c_void, CString};
use std::ptr;
use std::sync::Mutex;
use std::thread::{self, ThreadId};

use crate::error_code::error_code_for;
use crate::host::{lock, HostEvent, Origin};

/// cbindgen:prefix-with-name
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    /// A document was created or changed; `EventOrigin` says by whom.
    DocumentChanged = 1,
    DocumentDeleted = 2,
    SyncStarted = 3,
    /// Every subscribed scope caught up, once per connection. Not a promise that uploads are done.
    SyncCompleted = 4,
    SyncError = 5,
    /// Local content was set aside in Kept copies.
    ConflictDetected = 6,
    ConnectionLost = 7,
    ConnectionAttempted = 8,
    ConnectionSucceeded = 9,
    /// Changes were trimmed before this engine read them, or this handle fell more than 4096
    /// events behind: reload every list.
    DatabaseChanged = 11,
    /// The data dir adopted the signed-in account's user id and restamped its documents:
    /// re-read `replicant_get_user_id` and reload lists. On the sync callback.
    IdentityAdopted = 12,
}

/// cbindgen:prefix-with-name
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOrigin {
    /// A handle of this engine wrote it (this process, this copy of the library): another
    /// handle's write on the same engine is `Local` too.
    Local = 0,
    /// Sync wrote it: a download, a settle, a conflict revert or a sweep, in any process.
    Remote = 1,
    /// Another process, or another copy of the library in this process, on the same data dir.
    OtherProcess = 2,
}

impl From<Origin> for EventOrigin {
    fn from(origin: Origin) -> Self {
        match origin {
            Origin::Local => EventOrigin::Local,
            Origin::Server => EventOrigin::Remote,
            Origin::OtherProcess => EventOrigin::OtherProcess,
        }
    }
}

/// `DocumentChanged` / `DocumentDeleted`. For a deletion only `document_id` is set; for a change
/// `title`, `owner_id` and `author_id` may be null.
/// `visibility` is `public` (curated or read-only) or `private`.
/// Strings are valid only during the call; copy what you keep.
pub type ReplicantDocumentEventCallback = Option<
    extern "C" fn(
        event_type: EventType,
        document_id: *const c_char,
        title: *const c_char,
        content: *const c_char,
        owner_id: *const c_char,
        author_id: *const c_char,
        visibility: *const c_char,
        read_only: bool,
        origin: EventOrigin,
        context: *mut c_void,
    ),
>;

/// `ReplicantDocumentEventCallback` once registered (never null).
type DocumentFn = extern "C" fn(
    event_type: EventType,
    document_id: *const c_char,
    title: *const c_char,
    content: *const c_char,
    owner_id: *const c_char,
    author_id: *const c_char,
    visibility: *const c_char,
    read_only: bool,
    origin: EventOrigin,
    context: *mut c_void,
);

/// `SyncStarted`, `SyncCompleted`, `DatabaseChanged`, `IdentityAdopted`.
/// Strings are valid only during the call; copy what you keep.
pub type ReplicantSyncEventCallback =
    Option<extern "C" fn(event_type: EventType, context: *mut c_void)>;

/// `ReplicantSyncEventCallback` once registered (never null).
type SyncFn = extern "C" fn(event_type: EventType, context: *mut c_void);

/// `error_code` is a `ReplicantErrorCode`; `error` is the protocol code, e.g. "clock_skew";
/// `document_id` is null unless the error is about one document; `scope` is null unless it is
/// about one subscribed scope (e.g. `subscription_forbidden`); `fatal` means halted;
/// `recovered_id` names local content kept aside with the error (a Kept copies id, reasons as
/// in `ReplicantConflictEventCallback`), -1 if none.
/// Per engine: only the handles of the engine that applied the rule get this event; another
/// process, or another copy of the library, sees just `DocumentChanged`.
/// `replicant_list_recovered` is the durable record. `recovered_id` may already be dismissed or
/// restored by another process: `restore_*` then returns `ErrorNotFound`.
/// Strings are valid only during the call; copy what you keep.
pub type ReplicantErrorEventCallback = Option<
    extern "C" fn(
        event_type: EventType,
        error_code: i32,
        error: *const c_char,
        document_id: *const c_char,
        scope: *const c_char,
        fatal: bool,
        recovered_id: i64,
        context: *mut c_void,
    ),
>;

/// `ReplicantErrorEventCallback` once registered (never null).
type ErrorFn = extern "C" fn(
    event_type: EventType,
    error_code: i32,
    error: *const c_char,
    document_id: *const c_char,
    scope: *const c_char,
    fatal: bool,
    recovered_id: i64,
    context: *mut c_void,
);

/// `attempt_number` counts dials since the last successful connection.
/// Strings are valid only during the call; copy what you keep.
pub type ReplicantConnectionEventCallback = Option<
    extern "C" fn(
        event_type: EventType,
        connected: bool,
        attempt_number: u32,
        context: *mut c_void,
    ),
>;

/// `ReplicantConnectionEventCallback` once registered (never null).
type ConnectionFn = extern "C" fn(
    event_type: EventType,
    connected: bool,
    attempt_number: u32,
    context: *mut c_void,
);

/// `reason`: `conflict`, `field_conflict`, `delete_wins` or `delete_superseded` (a delete undone
/// because a newer version arrived). `recovered_id` is the Kept copies id, or -1 when nothing
/// was kept.
/// A kept copy's reason is one of `conflict`, `field_conflict`, `delete_wins`,
/// `delete_superseded`, `delete_refused`, `delete_publication`, `became_publication`,
/// `create_rejected` or `unmigratable` (set aside while upgrading the database, with no event).
/// `paths_json` is a JSON array of JSON Pointers for `field_conflict`, else null.
/// Per engine: only the handles of the engine that applied the rule get this event; another
/// process, or another copy of the library, sees just `DocumentChanged`.
/// `replicant_list_recovered` is the durable record. `recovered_id` may already be dismissed or
/// restored by another process: `restore_*` then returns `ErrorNotFound`.
/// Strings are valid only during the call; copy what you keep.
pub type ReplicantConflictEventCallback = Option<
    extern "C" fn(
        event_type: EventType,
        document_id: *const c_char,
        reason: *const c_char,
        recovered_id: i64,
        paths_json: *const c_char,
        context: *mut c_void,
    ),
>;

/// `ReplicantConflictEventCallback` once registered (never null).
type ConflictFn = extern "C" fn(
    event_type: EventType,
    document_id: *const c_char,
    reason: *const c_char,
    recovered_id: i64,
    paths_json: *const c_char,
    context: *mut c_void,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchError {
    NoCallbacks,
    WrongThread,
}

#[derive(Clone, Copy)]
struct Registered<F> {
    callback: F,
    context: *mut c_void,
}

#[derive(Clone, Default)]
struct Callbacks {
    thread: Option<ThreadId>,
    document: Option<(Registered<DocumentFn>, Option<EventType>)>,
    sync: Option<Registered<SyncFn>>,
    error: Option<Registered<ErrorFn>>,
    connection: Option<Registered<ConnectionFn>>,
    conflict: Option<Registered<ConflictFn>>,
}

// The contexts are only ever passed back to callbacks on the registering thread.
unsafe impl Send for Callbacks {}

#[derive(Default)]
pub struct Dispatcher(Mutex<Callbacks>);

/// Each `register_*` replaces the kind's previous callback and context; `None` removes it. The
/// first registration binds the calling thread; later ones from another thread are refused, so a
/// change never races a pump and applies from the next event on.
impl Dispatcher {
    /// `filter`: one of `DocumentChanged` / `DocumentDeleted`, or every document event.
    pub fn register_document(
        &self,
        callback: Option<DocumentFn>,
        context: *mut c_void,
        filter: Option<EventType>,
    ) -> Result<(), DispatchError> {
        self.bind()?.document = callback.map(|callback| (Registered { callback, context }, filter));
        Ok(())
    }

    pub fn register_sync(
        &self,
        callback: Option<SyncFn>,
        context: *mut c_void,
    ) -> Result<(), DispatchError> {
        self.bind()?.sync = callback.map(|callback| Registered { callback, context });
        Ok(())
    }

    pub fn register_error(
        &self,
        callback: Option<ErrorFn>,
        context: *mut c_void,
    ) -> Result<(), DispatchError> {
        self.bind()?.error = callback.map(|callback| Registered { callback, context });
        Ok(())
    }

    pub fn register_connection(
        &self,
        callback: Option<ConnectionFn>,
        context: *mut c_void,
    ) -> Result<(), DispatchError> {
        self.bind()?.connection = callback.map(|callback| Registered { callback, context });
        Ok(())
    }

    pub fn register_conflict(
        &self,
        callback: Option<ConflictFn>,
        context: *mut c_void,
    ) -> Result<(), DispatchError> {
        self.bind()?.conflict = callback.map(|callback| Registered { callback, context });
        Ok(())
    }

    fn bind(&self) -> Result<std::sync::MutexGuard<'_, Callbacks>, DispatchError> {
        let mut callbacks = lock(&self.0);
        let current = thread::current().id();
        if *callbacks.thread.get_or_insert(current) != current {
            return Err(DispatchError::WrongThread);
        }
        Ok(callbacks)
    }

    /// Runs the callbacks for the events `take` hands over, stopping early once `stop()` is
    /// true; returns the number delivered. Refused, and nothing taken, before any registration
    /// or off the registering thread. Each event reads the callbacks afresh, so a registration
    /// made by a callback applies to the rest of the batch.
    pub fn process(
        &self,
        take: impl FnOnce() -> Vec<HostEvent>,
        stop: impl Fn() -> bool,
    ) -> Result<usize, DispatchError> {
        self.may_process()?;
        let mut delivered = 0;
        for event in take() {
            if stop() {
                break;
            }
            let callbacks = lock(&self.0).clone();
            callbacks.dispatch(&event);
            delivered += 1;
        }
        Ok(delivered)
    }

    /// Whether `process` may run on this thread. The bound thread never changes once set.
    pub fn may_process(&self) -> Result<(), DispatchError> {
        match lock(&self.0).thread {
            None => Err(DispatchError::NoCallbacks),
            Some(registered) if registered != thread::current().id() => {
                Err(DispatchError::WrongThread)
            }
            Some(_) => Ok(()),
        }
    }
}

impl Callbacks {
    fn dispatch(&self, event: &HostEvent) {
        match event {
            HostEvent::DocumentChanged { document, origin } => {
                let id = text(&document.id.to_string());
                let title = document.title.as_deref().map(text);
                let content = text(&document.content.to_string());
                let owner = document.user_id.map(|owner| text(&owner.to_string()));
                let author = document.author_id.map(|author| text(&author.to_string()));
                let visibility = text(document.visibility);
                if let Some((entry, filter)) = &self.document {
                    if filter.is_none_or(|wanted| wanted == EventType::DocumentChanged) {
                        (entry.callback)(
                            EventType::DocumentChanged,
                            id.as_ptr(),
                            or_null(&title),
                            content.as_ptr(),
                            or_null(&owner),
                            or_null(&author),
                            visibility.as_ptr(),
                            document.read_only,
                            (*origin).into(),
                            entry.context,
                        );
                    }
                }
            }
            HostEvent::DocumentDeleted { doc_id, origin } => {
                let id = text(&doc_id.to_string());
                if let Some((entry, filter)) = &self.document {
                    if filter.is_none_or(|wanted| wanted == EventType::DocumentDeleted) {
                        (entry.callback)(
                            EventType::DocumentDeleted,
                            id.as_ptr(),
                            ptr::null(),
                            ptr::null(),
                            ptr::null(),
                            ptr::null(),
                            ptr::null(),
                            false,
                            (*origin).into(),
                            entry.context,
                        );
                    }
                }
            }
            HostEvent::SyncStarted => self.sync_event(EventType::SyncStarted),
            HostEvent::SyncCompleted => self.sync_event(EventType::SyncCompleted),
            HostEvent::DatabaseChanged => self.sync_event(EventType::DatabaseChanged),
            HostEvent::IdentityAdopted { .. } => self.sync_event(EventType::IdentityAdopted),
            HostEvent::SyncError {
                code,
                doc_id,
                scope,
                fatal,
                recovered_id,
            } => {
                let error = text(code);
                let document_id = doc_id.map(|id| text(&id.to_string()));
                let scope = scope.as_deref().map(text);
                if let Some(entry) = &self.error {
                    (entry.callback)(
                        EventType::SyncError,
                        error_code_for(code) as i32,
                        error.as_ptr(),
                        or_null(&document_id),
                        or_null(&scope),
                        *fatal,
                        recovered_id.unwrap_or(-1),
                        entry.context,
                    );
                }
            }
            HostEvent::Conflict {
                doc_id,
                reason,
                recovered_id,
                paths,
            } => {
                let id = text(&doc_id.to_string());
                let reason = text(reason);
                let paths_json = (!paths.is_empty())
                    .then(|| text(&serde_json::to_string(paths).unwrap_or_default()));
                if let Some(entry) = &self.conflict {
                    (entry.callback)(
                        EventType::ConflictDetected,
                        id.as_ptr(),
                        reason.as_ptr(),
                        recovered_id.unwrap_or(-1),
                        or_null(&paths_json),
                        entry.context,
                    );
                }
            }
            HostEvent::ConnectionAttempted { attempt } => {
                self.connection_event(EventType::ConnectionAttempted, false, *attempt)
            }
            HostEvent::ConnectionSucceeded => {
                self.connection_event(EventType::ConnectionSucceeded, true, 0)
            }
            HostEvent::ConnectionLost => self.connection_event(EventType::ConnectionLost, false, 0),
        }
    }

    fn sync_event(&self, event_type: EventType) {
        if let Some(entry) = &self.sync {
            (entry.callback)(event_type, entry.context);
        }
    }

    fn connection_event(&self, event_type: EventType, connected: bool, attempt: u32) {
        if let Some(entry) = &self.connection {
            (entry.callback)(event_type, connected, attempt, entry.context);
        }
    }
}

/// Interior NULs cannot cross into C; such a value arrives empty.
fn text(value: &str) -> CString {
    CString::new(value).unwrap_or_default()
}

fn or_null(value: &Option<CString>) -> *const c_char {
    value.as_ref().map_or(ptr::null(), |text| text.as_ptr())
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;

    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::store::StoredDocument;

    type Log = Mutex<Vec<String>>;

    fn log(context: *mut c_void) -> &'static Log {
        unsafe { &*(context as *const Log) }
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

    extern "C" fn on_document(
        event_type: EventType,
        document_id: *const c_char,
        title: *const c_char,
        _content: *const c_char,
        owner_id: *const c_char,
        _author_id: *const c_char,
        visibility: *const c_char,
        read_only: bool,
        origin: EventOrigin,
        context: *mut c_void,
    ) {
        log(context).lock().unwrap().push(format!(
            "{event_type:?} {} {} {} {} {read_only} {origin:?}",
            read(document_id),
            read(title),
            read(owner_id),
            read(visibility)
        ));
    }

    extern "C" fn on_sync(event_type: EventType, context: *mut c_void) {
        log(context).lock().unwrap().push(format!("{event_type:?}"));
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

    extern "C" fn on_connection(
        event_type: EventType,
        connected: bool,
        attempt_number: u32,
        context: *mut c_void,
    ) {
        log(context)
            .lock()
            .unwrap()
            .push(format!("{event_type:?} {connected} {attempt_number}"));
    }

    extern "C" fn on_conflict(
        event_type: EventType,
        document_id: *const c_char,
        reason: *const c_char,
        recovered_id: i64,
        paths_json: *const c_char,
        context: *mut c_void,
    ) {
        log(context).lock().unwrap().push(format!(
            "{event_type:?} {} {} {recovered_id} {}",
            read(document_id),
            read(reason),
            read(paths_json)
        ));
    }

    #[test]
    fn every_host_event_reaches_its_callback() {
        let seen: Log = Mutex::new(Vec::new());
        let context = &seen as *const Log as *mut c_void;
        let dispatcher = Dispatcher::default();
        dispatcher
            .register_document(Some(on_document), context, None)
            .unwrap();
        dispatcher.register_sync(Some(on_sync), context).unwrap();
        dispatcher.register_error(Some(on_error), context).unwrap();
        dispatcher
            .register_connection(Some(on_connection), context)
            .unwrap();
        dispatcher
            .register_conflict(Some(on_conflict), context)
            .unwrap();
        let doc_id = Uuid::from_u128(1);
        let owner = Uuid::from_u128(0xA);
        let document = StoredDocument {
            id: doc_id,
            user_id: Some(owner),
            author_id: None,
            title: Some("T".into()),
            content: json!({"title": "T"}),
            read_only: false,
            visibility: "private",
            source_doc_id: None,
            derived_from: None,
            created_at: "c".into(),
            updated_at: "u".into(),
        };
        let events = vec![
            HostEvent::ConnectionAttempted { attempt: 1 },
            HostEvent::ConnectionSucceeded,
            HostEvent::SyncStarted,
            HostEvent::DocumentChanged {
                document,
                origin: Origin::OtherProcess,
            },
            HostEvent::DocumentDeleted {
                doc_id,
                origin: Origin::Server,
            },
            HostEvent::Conflict {
                doc_id,
                reason: "field_conflict".into(),
                recovered_id: Some(3),
                paths: vec!["/s".into()],
            },
            HostEvent::SyncError {
                code: "clock_skew".into(),
                doc_id: None,
                scope: None,
                fatal: false,
                recovered_id: None,
            },
            HostEvent::SyncError {
                code: "subscription_forbidden".into(),
                doc_id: None,
                scope: Some("collection:curated".into()),
                fatal: false,
                recovered_id: None,
            },
            HostEvent::SyncError {
                code: "forbidden".into(),
                doc_id: Some(doc_id),
                scope: None,
                fatal: false,
                recovered_id: Some(4),
            },
            HostEvent::SyncCompleted,
            HostEvent::DatabaseChanged,
            HostEvent::IdentityAdopted { user_id: owner },
            HostEvent::ConnectionLost,
        ];
        assert_eq!(dispatcher.process(|| events, || false), Ok(13));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                "ConnectionAttempted false 1".to_string(),
                "ConnectionSucceeded true 0".to_string(),
                "SyncStarted".to_string(),
                format!("DocumentChanged {doc_id} T {owner} private false OtherProcess"),
                format!("DocumentDeleted {doc_id} null null null false Remote"),
                format!("ConflictDetected {doc_id} field_conflict 3 [\"/s\"]"),
                "SyncError 2101 clock_skew null null false -1".to_string(),
                "SyncError 3004 subscription_forbidden null collection:curated false -1"
                    .to_string(),
                format!("SyncError 5003 forbidden {doc_id} null false 4"),
                "SyncCompleted".to_string(),
                "DatabaseChanged".to_string(),
                "IdentityAdopted".to_string(),
                "ConnectionLost false 0".to_string(),
            ]
        );
    }

    #[test]
    fn processing_before_registration_or_off_thread_is_refused_and_takes_nothing() {
        let dispatcher = Dispatcher::default();
        assert_eq!(
            dispatcher.process(
                || unreachable!("nothing is taken before a registration"),
                || false
            ),
            Err(DispatchError::NoCallbacks)
        );
        let seen: Log = Mutex::new(Vec::new());
        dispatcher
            .register_sync(Some(on_sync), &seen as *const Log as *mut c_void)
            .unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                assert_eq!(
                    dispatcher
                        .process(|| unreachable!("nothing is taken off the thread"), || false),
                    Err(DispatchError::WrongThread)
                );
            });
        });
        assert_eq!(
            dispatcher.process(|| vec![HostEvent::SyncStarted], || false),
            Ok(1)
        );
        assert_eq!(*seen.lock().unwrap(), vec!["SyncStarted".to_string()]);
    }
}
