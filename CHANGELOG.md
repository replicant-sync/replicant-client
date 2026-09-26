# Changelog

## 0.6.4

Patch release: no API change. Event timing changes for hosts that relied on the
start-up events (#45).

- ConnectionSucceeded and SyncCompleted are emitted only when they happen.
  `replicant_create` no longer emits them after background init, so an offline
  engine or a local-only engine (no API key, or a database that never adopted an
  identity) reports neither. SyncCompleted comes only from the server's
  `SyncComplete` and is always preceded by SyncStarted, including after a
  reconnect.
- The engine is usable before it announces a connection. Start-up is split into
  connect and start, so documents written by the host during start-up are
  uploaded by the initial sync instead of staying pending until the next
  reconnect.
- A failed initial sync leaves a working client: upload protection is cleared,
  failed sends release their in-flight entry, and the failure is reported as
  SyncError.
- The reconnection monitor stops on shutdown, and a shut-down client stays
  disconnected even when a reconnect handshake completes after `shutdown()`.

## 0.6.3

Patch release: `replicant_destroy` keeps its signature and stays non-blocking,
and the new wait entry point is purely additive, so no API change.

Release with `cargo release 0.6.3`, naming the version explicitly: `cargo release
patch` would produce 0.6.2, and that tag already exists — v0.6.2 points at a CI
commit that carried no version bump, which is also why `Cargo.toml` still reads
0.6.1.

- `replicant_destroy` now actually tears the instance down instead of just
  dropping it, and the teardown closes both sqlite pools and joins the threads
  the handle started (DEV-1118). Previously destroy was a plain drop: the tokio
  runtime went down (cancelling the background init task) and the pools were
  dropped without being closed, leaving sqlx's per-connection worker threads —
  which sqlx spawns detached and never joins — running code inside the caller's
  module. A host that unloaded the module at that point faulted on an unmapped
  image: a `0xC0000005` in Entonal Studio 2's CLAP plugin about one
  clap-validator run in nine, but only in the validator's parallel mode, where
  several processes contend on one database and SQLITE_BUSY keeps a worker alive
  past the instance that started it.

  The teardown waits for the background init task (rather than cancelling it
  mid-connect), awaits `Pool::close()` on both pools, and drops the runtime so
  its workers are joined. It runs on a dedicated `replicant-shutdown` thread and
  is deliberately unbounded, because every bound would mean abandoning a pool
  mid-close — the very thing it exists to prevent.

  `replicant_destroy` keeps its signature and stays non-blocking: it starts that
  teardown and returns at once, so a plugin host can call it on a UI thread
  during a scan or project close without being stalled by another process's lock.
  A `replicant_create` that fails now also closes the pool it opened, since no
  handle comes back for anyone to destroy.

- New `bool replicant_destroy_and_wait(handle, timeout_ms)`: same teardown, but
  waits up to `timeout_ms` for it and returns whether it finished — `true` means
  no Replicant thread is left and unloading the module is safe. `0` does not wait
  at all; `UINT32_MAX` waits in effect indefinitely. Additive; for standalone
  apps and tests. Plugins should keep using `replicant_destroy`.

- Because `replicant_destroy` is fire-and-forget, Replicant-owned threads can
  briefly outlive it, so a caller that a host can unload MUST pin its own module:
  Windows `GetModuleHandleExW` with `GET_MODULE_HANDLE_EX_FLAG_PIN`, POSIX
  `dlopen(RTLD_NOLOAD | RTLD_NODELETE)` on its own image. That is now the
  documented primary defence (see `replicant.h`), not a workaround — the pools
  being closed and joined shrinks the window from seconds to microseconds, it
  does not remove it, because sqlx exposes no way to join its worker threads.

- **Upgrading without pinning is a regression.** 0.6.2's destroy dropped the
  handle, which joined the tokio runtime's threads before returning and left only
  sqlx's workers running — forever. This one returns with the background init,
  both pools, the runtime's threads and the sqlx workers all still live, and then
  closes and joins them. Total hazard duration goes down; exposure at the moment
  destroy returns goes up. The pin is what covers it.

- `replicant_destroy` returning early means the outgoing instance may still be
  writing to the database for a moment, so a host that re-instantiates
  immediately can meet sqlite contention, and `replicant_create` returns NULL if
  that outlasts the 5s busy timeout. `replicant_destroy_and_wait` avoids the
  overlap. Documented on both calls in `replicant.h`.

- `Client::shutdown` closes its pool before touching the socket. The `ws_client`
  guard is held across un-timed network sends by live background tasks, so making
  it a precondition would let a half-open socket keep every sqlx worker thread
  alive for the life of the process — the failure this change exists to remove.
  The socket is now released best-effort with a 500ms bound.

- sqlite worker threads are now named `replicant-sqlite-N` and counted, so the
  next crash dump says whose threads they are.

- The destruction contract is documented where integrators actually read it:
  `replicant.hpp`'s `Client`, whose `unique_ptr` deleter is what calls
  `replicant_destroy`, not only the generated `replicant.h`.

- A `Client` whose construction fails now closes the pool it opened instead of
  dropping it un-closed, which left one unjoined sqlite worker thread per
  connection running for the life of the process. Same fix as for a failed
  `replicant_create`, on the background path — and this is the failure mode the
  crash was reported against: parallel instances whose migrations outlast
  sqlite's busy timeout.

## 0.6.2 - 2026-09-13

- CI publishes a linux-arm64 SDK asset. No library changes; the tag carried no
  version bump, so `Cargo.toml` remained at 0.6.1.

## 0.6.1 - 2026-09-02

- Release macOS libs now pin `MACOSX_DEPLOYMENT_TARGET` per architecture
  (10.15 for x86_64, 11.0 for arm64) so the universal `libreplicant_client.a`
  no longer triggers "was built for newer macOS version" link warnings in
  hosts targeting older deployment targets (DEV-1079).

## 0.6.0 - 2026-09-01

Document events carry an origin flag (#44). All workspace crates move to 0.6.0 together.

### Breaking

- Document events now carry their origin. `DocumentEventCallback` in
  `replicant.h` takes a new `ReplicantEventOrigin origin` argument before
  `context`, so every C/C++ callback must be updated; the JUCE wrapper's
  `onDocumentCreated`/`onDocumentUpdated`/`onDocumentDeleted` gained the same
  trailing parameter. In Rust, `SyncEvent::Document{Created,Updated,Deleted}`
  gained an `origin: EventOrigin` field, and
  `emit_document_{created,updated}_with_attribution` / `emit_document_deleted`
  take it explicitly.
- `origin` is `Local` when this client wrote the document itself and `Remote`
  when the change was applied from the server (a broadcast from another client
  or instance, or a sync pass). Events for a client's own writes are still
  emitted — separate clients over one database depend on them — so a consumer
  that only cares about changes made elsewhere must check `origin` rather than
  comparing content. (#44)

## 0.5.0 - 2026-08-28

Sync-base verification (DEV-1037). All workspace crates move to 0.5.0 together.

### Breaking

- `DocumentUpdated` payload handling changed. `ServerMessage::DocumentUpdatedResponse`
  now carries `reason`, `current_revision`, `current_content` and `current_hash`, and
  `update_document` returns the structured rejection instead of a stringified error.
  Consumers that matched on the old error string must move to the typed fields.
- New C-ABI error code `UpdateConflict = 5001` (`replicant.h`). A `hash_mismatch`
  that cannot be rebased is now surfaced to the host as this code, where it
  previously appeared as a generic error. `5xxx` means unresolved divergence:
  show it to the user and do not retry.

### Fixed

- Broadcast apply is guarded and idempotent, checks revision continuity, and
  verifies the content hash after applying.
- Targeted resync via the server's `get_document` op (own and public documents)
  with bounded retries, replacing full-sync fallbacks on a revision gap.
- An upload rejected with `hash_mismatch` rebases the queued patch and resends,
  capped at three attempts per document per session; beyond that the document
  enters a durable `Conflict` state whose content keeps following the server
  until the next local edit.
- The offline queue holds a single row per document carrying the cumulative diff
  against the stored base, so a batch of offline edits flushes as one patch
  against the revision the server actually holds.
- Server echoes of a client's own broadcasts are deferred while that document
  has an upload in flight.
- Adopting writes clear the sync queue in the same transaction as the
  compare-and-swap, so a crash cannot leave a document written but still queued.

### Testing

- `three_client_convergence_torture`: three replicas of one user run 30 seeded,
  randomized, interleaved operations with one replica toggling offline twice,
  then must converge bit-identically with the server. Set `TORTURE_SEED` to
  replay or explore an interleaving.
- The Phoenix interop harness now pins `replicant-server` at `dad45e5` by default.
