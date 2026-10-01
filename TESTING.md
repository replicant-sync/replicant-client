# Testing

- Offline suites: `cargo test -p replicant-client` (unit, `ffi_tests`, `ffi_lifecycle_tests`, `wire_contract_tests`, `two_process`).
- Interop: `test/run_phoenix_interop_local.sh` boots replicant-server `facd572` on a throwaway database (`replicant_interop_v2`) and runs the ignored `interop`, `v2_smoke` and `two_process` tests (`INTEROP_TEST_CMD` overrides the command; CI job `interop`).
- Local run: `REPLICANT_SERVER_DIR=<detached server worktree> INTEROP_SERVER_PORT=4100 test/run_phoenix_interop_local.sh` (one test waits 60 s for a silent drop).
- `scripts/release_link_probe.c` links the library like a release consumer (dead-strip, strip) and CI job `release-link` checks the binary still carries the version marker (Linux, macOS, Windows).
