# Replicant client

An offline-first JSON document sync library in Rust, with a C API, for the Replicant sync server.

## Build

    cargo build --release
    python3 scripts/build_dist.py

`build_dist.py` puts the headers (`replicant.h`, `replicant.hpp`) and the libraries in `dist/`.

## C API

`#include "replicant.h"` for C, or `replicant.hpp` for C++ (`replicant::Client`, a thin RAII wrapper that throws `SyncException`). The header's comments are the reference. The ABI is `REPLICANT_ABI_VERSION_MAJOR.MINOR`: a breaking change bumps the major, an addition the minor; `replicant_abi_version` returns the library's, packed `(major << 16) | minor`.

- `replicant_create` attaches to the engine for a data dir; every handle in the process on that data dir shares it. It never waits on the network. Set `struct_size` in `ReplicantConfig`. `ErrorNewerSchema` means a newer build migrated the database; `ErrorMigrationFailed` that a v1 library could not be upgraded (either the backup failed and nothing changed, or the migration failed after a `.v1-backup` copy was kept, `.v1-backup-<unix seconds>` when that name was taken; an existing backup is never replaced); `ErrorBusy` that another process kept it locked; `ErrorConfigMismatch` that an open engine uses another config.
- `ReplicantConfig.list_merge` and `list_merge_rules_json` choose how a list changed on both sides is merged: `Append` (default) merges element by element while positions line up, `Atomic` keeps the server's list and sets the local one aside. `Full` is refused for now. Every handle on a data dir must pass the same config.
- Reads and writes are local SQLite; the engine uploads and downloads in the background. Never call from an audio thread.
- Sign-in: `replicant_enroll_request` emails a code, `replicant_enroll_claim` exchanges it and stores the credentials itself (it returns only the user id). `replicant_clear_credentials` signs out. Both apply at once to open engines on that data dir in this process; engines in other processes pick up a sign-in within about 3 s and a sign-out within about a second. The api key and secret never cross the C API: a host tells whether it is signed in from `replicant_get_state` (`halt_reason` is `NotEnrolled` when signed out) and reads the user id with `replicant_get_user_id`.
- `replicant_get_state` gives connection, sync phase and halt reason; `replicant_reconnect` retries.
- Kept copies: `replicant_list_recovered`, `replicant_restore_document`, `replicant_restore_fields`, `replicant_dismiss_recovered`. Documents the server refused to change: `replicant_list_parked`.
- Events: see `EVENT_CALLBACKS.md`.
- Plugins must never unload the library while the process runs: `replicant_destroy` returns at once and the engine stops afterwards.

## Release gate

Every binary that links replicant-client carries `replicant-client-version=<version>`. Before a release, check that all of them link the same version:

    python3 scripts/check_replicant_versions.py <app bundle> <plugin bundles…>

It exits 1 when a binary has no marker, carries two versions, or the binaries disagree.

## License

MIT
