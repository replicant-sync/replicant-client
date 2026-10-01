# Events

Register callbacks on a handle, then call `replicant_process_events` from the same thread (usually the host's main or UI timer). The first registration fixes that thread; calls from any other thread are refused (`ErrorWrongThread`) and nothing is lost. Callbacks run only inside `replicant_process_events`; their strings are valid only during the call. Never call the library from an audio thread.

| Callback | Events |
|---|---|
| document | `DocumentChanged`, `DocumentDeleted` (filter: -1 all, 1, 2) |
| sync | `SyncStarted`, `SyncCompleted`, `DatabaseChanged`, `IdentityAdopted` (re-read `replicant_get_user_id`, reload lists) |
| connection | `ConnectionAttempted`, `ConnectionSucceeded`, `ConnectionLost` |
| error | `SyncError` (`error_code`, protocol code, document id or null, `fatal`, `recovered_id` or -1) |
| conflict | `ConflictDetected` (`reason`, `recovered_id` or -1, `paths_json` for field conflicts) |

- **Kept copies in events:** `ConflictDetected` reasons are `conflict`, `field_conflict`, `delete_wins` and `delete_superseded` (a delete undone because a newer version arrived; the document is back, and a copy is kept only if it was edited before the delete). `SyncError` carries a copy for `became_publication`, `create_rejected`, `delete_publication` (a delete undone because the document became a publication; the document is back, read-only, and the edits made before the delete are kept; the error code is still `became_publication`), and a delete the server refused (its protocol code is the server's, e.g. `forbidden`; the document is back, and edits made before the delete are kept as `delete_refused`). `diverged` (5007) means the document stopped uploading until its next edit; nothing is lost.

- **Origins:** `Local` (a handle of this engine wrote it), `Remote` (sync wrote it, in any process), `OtherProcess` (another process or library copy on the same data dir).
- **One event per document per pass**, to every handle, the writer included.
- **Per connection:** `ConnectionSucceeded` → `SyncStarted` → `SyncCompleted` (at most once) → `ConnectionLost`. A handle created mid-connection first receives the events of that connection so far. Dials not yet delivered with no other event between them arrive as one `ConnectionAttempted` (the latest); all events keep their order.
- **`DatabaseChanged`:** changes were trimmed before this engine read them, or this handle fell 4096 events behind (its document events were collapsed); reload every list.
- **Conflicts** are raised only in the process that applied the rule; every process sees the kept copy in `replicant_list_recovered`, the source of truth. A `recovered_id` may already be dismissed or restored elsewhere (`ErrorNotFound`).
- **State** for a status indicator comes from `replicant_get_state`, not from counting events; call it right after `replicant_create`. Parked documents come from `replicant_list_parked` and are not in `replicant_count_pending_sync`.
- **Destroy** inside a callback is allowed: the handle is freed when `replicant_process_events` returns. Anywhere else a destroy must not overlap any other call on that handle, on any thread.
