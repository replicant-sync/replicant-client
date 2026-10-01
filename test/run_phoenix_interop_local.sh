#!/bin/bash
#
# Phoenix v2 interop runner (Rust client <-> Elixir replicant-server).
#
# replicant-server is a LIBRARY: it ships the sync socket
# (`lib/replicant_server/sync/socket.ex`) but no HTTP server of its own;
# production hosts (entonal-web-app) mount the socket in their own endpoint.
# This harness supplies that host: it boots the server's `TestEndpoint` (which
# mounts `ReplicantServer.Sync.Socket`) on a local port, seeds one enrolled user
# + credential, one legacy nil-user credential and one curated publication
# through the server's own modules, and runs the client's interop suites
# against it on a throwaway database.
#
# Usage:
#   test/run_phoenix_interop_local.sh                 # run the full suite
#   test/run_phoenix_interop_local.sh test_name       # run a single test filter
#
# Environment:
#   INTEROP_TEST_CMD       command to run instead of the cargo suites (consumer
#                          suites, e.g. entonal-common); gets the seeded env
#   INTEROP_SEED_USER_ID   fixed user id for the enrolled user
#   INTEROP_IMPORT_DOCS    JSON file of [{id, content}] to create for the user
#   INTEROP_DB_NAME        throwaway database to drop and recreate (replicant_interop_v2)
#   REPLICANT_SERVER_REF   server SHA to test
#
# Requires: a running PostgreSQL, Elixir/mix, cargo, and curl.

set -euo pipefail

# --- Paths -------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLIENT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CLIENT_CRATE="$CLIENT_ROOT/replicant-client"

# --- Server pin --------------------------------------------------------------
# replicant-server main: protocol v2 (#10) + hardening (#11), 0.5.0.
SERVER_REF="${REPLICANT_SERVER_REF:-facd572}"

# Where to prepare the server checkout. Locally we make a detached git worktree
# from a sibling replicant-server clone; in CI (no local clone) we git-clone.
SERVER_SRC="${REPLICANT_SERVER_SRC:-$CLIENT_ROOT/../replicant-server}"
SERVER_CLONE_URL="${REPLICANT_SERVER_CLONE_URL:-https://github.com/replicant-sync/replicant-server.git}"
SERVER_DIR="${REPLICANT_SERVER_DIR:-/tmp/replicant-server-interop}"

# Hex/Mix caches. Redirected to writable paths so `mix deps.get` can persist its
# registry cache even where $HOME/.hex is not writable (sandboxed shells).
export HEX_HOME="${INTEROP_HEX_HOME:-/tmp/replicant-interop-hex}"
export MIX_HOME="${INTEROP_MIX_HOME:-/tmp/replicant-interop-mix}"
mkdir -p "$HEX_HOME" "$MIX_HOME"

# --- Configuration -----------------------------------------------------------
DB_NAME="${INTEROP_DB_NAME:-replicant_interop_v2}"
DB_USER="${INTEROP_DB_USER:-postgres}"
DB_PASS="${INTEROP_DB_PASS:-postgres}"
DB_HOST="${INTEROP_DB_HOST:-localhost}"
SERVER_PORT="${INTEROP_SERVER_PORT:-4000}"
DATABASE_URL="ecto://$DB_USER:$DB_PASS@$DB_HOST/$DB_NAME"
SERVER_LOG="${INTEROP_SERVER_LOG:-/tmp/replicant_interop_server.log}"
TEST_EMAIL="${INTEROP_TEST_EMAIL:-integration-test@example.com}"
export MIX_ENV=test

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +'%H:%M:%S')] $1${NC}"; }
warn() { echo -e "${YELLOW}[$(date +'%H:%M:%S')] WARN: $1${NC}"; }
err()  { echo -e "${RED}[$(date +'%H:%M:%S')] ERROR: $1${NC}"; }

SERVER_PID=""
BOOT_SCRIPT=""
cleanup() {
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        log "Stopping server (PID $SERVER_PID)"
        kill "$SERVER_PID" 2>/dev/null || true
        sleep 1
        kill -9 "$SERVER_PID" 2>/dev/null || true
    fi
    local pids
    pids="$(lsof -ti :"$SERVER_PORT" 2>/dev/null || true)"
    [ -n "$pids" ] && kill -9 $pids 2>/dev/null || true
    [ -n "$BOOT_SCRIPT" ] && rm -f "$BOOT_SCRIPT"
}
trap cleanup EXIT INT TERM

# --- Preflight ---------------------------------------------------------------
command -v mix   >/dev/null || { err "mix not found on PATH"; exit 1; }
command -v cargo >/dev/null || { err "cargo not found on PATH"; exit 1; }
command -v curl  >/dev/null || { err "curl not found on PATH"; exit 1; }
if ! PGPASSWORD="$DB_PASS" psql -U "$DB_USER" -h "$DB_HOST" -d postgres -c "SELECT 1;" >/dev/null 2>&1; then
    err "PostgreSQL not reachable as $DB_USER@$DB_HOST"; exit 1
fi

existing="$(lsof -ti :"$SERVER_PORT" 2>/dev/null || true)"
[ -n "$existing" ] && { warn "Killing processes on port $SERVER_PORT: $existing"; kill -9 $existing 2>/dev/null || true; sleep 1; }

# --- Prepare pinned server checkout ------------------------------------------
if [ -d "$SERVER_DIR/.git" ] || [ -f "$SERVER_DIR/.git" ]; then
    log "Reusing server checkout at $SERVER_DIR (pinning to $SERVER_REF)"
    # Discard prior harness-injected mix.exs/mix.lock changes so checkout doesn't abort.
    git -C "$SERVER_DIR" checkout --quiet -- mix.exs mix.lock 2>/dev/null || true
    git -C "$SERVER_DIR" checkout --quiet --detach "$SERVER_REF" 2>/dev/null || \
        git -C "$SERVER_DIR" checkout --quiet "$SERVER_REF"
elif [ -d "$SERVER_SRC/.git" ]; then
    log "Creating detached worktree at $SERVER_DIR from $SERVER_SRC @ $SERVER_REF"
    git -C "$SERVER_SRC" worktree prune
    git -C "$SERVER_SRC" worktree add --force --detach "$SERVER_DIR" "$SERVER_REF"
else
    log "Cloning $SERVER_CLONE_URL into $SERVER_DIR @ $SERVER_REF"
    git clone "$SERVER_CLONE_URL" "$SERVER_DIR"
    git -C "$SERVER_DIR" checkout "$SERVER_REF"
fi

# The stripped server ships no HTTP adapter dependency (its own channel tests run
# with `server: false`). Inject Bandit so the endpoint can actually bind a WS
# port — mirroring what a production host app brings. Harness-local only.
if ! grep -q ':bandit' "$SERVER_DIR/mix.exs"; then
    log "Injecting Bandit HTTP adapter dependency (harness-only)"
    perl -0pi -e 's/(\{:jsonpatch,\s*"[^"]*"\})/$1,\n      {:bandit, "~> 1.0"}/' "$SERVER_DIR/mix.exs"
    grep -q ':bandit' "$SERVER_DIR/mix.exs" || { err "Failed to inject bandit dep"; exit 1; }
fi

# The harness serves real, long-lived websocket clients, not ExUnit tests. Under
# the Sandbox pool each channel process owns a connection for its whole lifetime,
# so N clients pin 2N connections (sync:user + sync:public). Use the normal pool.
if grep -q 'Ecto.Adapters.SQL.Sandbox' "$SERVER_DIR/config/test.exs"; then
    log "Switching Repo to the standard connection pool (harness-only)"
    perl -0pi -e 's/\s*pool: Ecto\.Adapters\.SQL\.Sandbox,//' "$SERVER_DIR/config/test.exs"
    perl -0pi -e 's/pool_size: System\.schedulers_online\(\) \* 2/pool_size: 20/' "$SERVER_DIR/config/test.exs"
fi

# config/test.exs hard-codes the database name; make it read INTEROP_DB_NAME so
# the harness never drops or recreates the server's own replicant_server_test.
if ! grep -q 'INTEROP_DB_NAME' "$SERVER_DIR/config/test.exs"; then
    log "Pointing the test Repo at INTEROP_DB_NAME (harness-only)"
    perl -pi -e 's/^(\s*)database: .*$/$1database: System.fetch_env!("INTEROP_DB_NAME"),/' "$SERVER_DIR/config/test.exs"
    grep -q 'INTEROP_DB_NAME' "$SERVER_DIR/config/test.exs" || { err "Failed to point the Repo at INTEROP_DB_NAME"; exit 1; }
fi
export INTEROP_DB_NAME="$DB_NAME"

# --- Build server ------------------------------------------------------------
log "Fetching + compiling server deps (MIX_ENV=test)"
( cd "$SERVER_DIR" && mix deps.get >/dev/null && mix compile >/dev/null )

# --- Clean database ----------------------------------------------------------
log "Recreating clean database '$DB_NAME'"
export DATABASE_URL
( cd "$SERVER_DIR"
  mix ecto.drop --quiet 2>/dev/null || true
  mix ecto.create --quiet
  mix ecto.migrate >/dev/null )

# --- Seed credentials --------------------------------------------------------
# One enrolled user+credential (bound user_id) via the enrollment flow, one
# legacy nil-user credential for the negative test, one curated publication, and
# optionally documents imported for the user from INTEROP_IMPORT_DOCS.
log "Seeding enrolled + legacy credentials for $TEST_EMAIL (stderr: $SERVER_LOG)"
: > "$SERVER_LOG"
SEED_LINE="$( cd "$SERVER_DIR" && TEST_EMAIL="$TEST_EMAIL" \
  INTEROP_SEED_USER_ID="${INTEROP_SEED_USER_ID:-}" \
  INTEROP_IMPORT_DOCS="${INTEROP_IMPORT_DOCS:-}" mix run -e '
  alias ReplicantServer.{Auth, Repo, Documents}
  email = System.get_env("TEST_EMAIL")
  case System.get_env("INTEROP_SEED_USER_ID") do
    nil -> :ok
    "" -> :ok
    id -> Repo.insert!(%ReplicantServer.Accounts.User{id: id, email: Auth.normalize_email(email)})
  end
  {:ok, token} = Auth.request_enrollment(email)
  {:ok, creds} = Auth.claim_enrollment(email, token)
  {:ok, legacy} = Auth.create_credential("interop-legacy-shared")
  curated_id = Ecto.UUID.generate()
  {:ok, _} = Documents.create_public_document(%{id: curated_id, content: %{"title" => "Curated seed", "n" => 1}})
  imported =
    case System.get_env("INTEROP_IMPORT_DOCS") do
      nil -> 0
      "" -> 0
      path ->
        path
        |> File.read!()
        |> Jason.decode!()
        |> Enum.map(fn %{"id" => id, "content" => c} ->
          {:ok, _} = Documents.create_document(creds.user_id, %{id: id, content: c})
        end)
        |> length()
    end
  IO.puts("SEED #{creds.api_key} #{creds.secret} #{creds.user_id} #{legacy.api_key} #{legacy.secret} #{curated_id} #{imported}")
' 2>>"$SERVER_LOG" | grep '^SEED ' )"
read -r _ API_KEY API_SECRET TEST_USER_ID LEGACY_API_KEY LEGACY_API_SECRET CURATED_ID IMPORTED <<<"$SEED_LINE"
[ -n "$API_KEY" ] && [ -n "$API_SECRET" ] && [ -n "$TEST_USER_ID" ] && \
[ -n "$LEGACY_API_KEY" ] && [ -n "$LEGACY_API_SECRET" ] && [ -n "$CURATED_ID" ] || {
    err "Failed to seed credentials"; echo "$SEED_LINE"; tail -30 "$SERVER_LOG"; exit 1; }
log "Enrolled user_id=$TEST_USER_ID (imported $IMPORTED documents)"

# --- Start server (minimal endpoint mounting the sync socket) ----------------
BOOT_SCRIPT="$(mktemp /tmp/replicant_interop_boot.XXXXXX.exs)"
cat > "$BOOT_SCRIPT" <<EOF
Application.put_env(:replicant_server, ReplicantServer.Sync.TestEndpoint,
  adapter: Bandit.PhoenixAdapter,
  http: [ip: {127, 0, 0, 1}, port: $SERVER_PORT],
  server: true,
  secret_key_base: "oD6r/Ez+1r8Dh1dGG7dZ8BQS3wcNOYQsXgrATKe1LCimCFRoO346xxuWJBbga1bE",
  pubsub_server: ReplicantServer.PubSub
)
{:ok, _} = ReplicantServer.Sync.TestEndpoint.start_link([])
IO.puts("ENDPOINT_STARTED")
Process.sleep(:infinity)
EOF

log "Starting minimal endpoint on port $SERVER_PORT (log: $SERVER_LOG)"
( cd "$SERVER_DIR" && exec mix run --no-halt "$BOOT_SCRIPT" ) >> "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

# Health-check: wait for the socket port to accept HTTP connections. The stripped
# server has no /health route, so any HTTP response (curl exit 0) means "up".
for i in $(seq 1 60); do
    if curl -s -o /dev/null --max-time 2 "http://127.0.0.1:$SERVER_PORT/" 2>/dev/null; then
        log "Server is up"; break
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then err "Server exited early:"; tail -30 "$SERVER_LOG"; exit 1; fi
    sleep 1
    [ "$i" -eq 60 ] && { err "Server did not come up within 60s"; tail -30 "$SERVER_LOG"; exit 1; }
done

# --- Run interop suite -------------------------------------------------------
# INTEROP_TEST_CMD lets a consumer suite (e.g. entonal-common's
# TonalDBSyncIntegrationTest) run under this harness's boot/seed/teardown in
# place of the cargo suites. It runs with the same seeded-credential env.
# --include-ignored also runs two_process's offline test; the child_* tests are
# skipped because they only do work when a two_process test spawns them.
log "Running ${INTEROP_TEST_CMD:-interop suites} against clean DB"
set +e
( cd "$CLIENT_CRATE" && \
  REPLICANT_API_KEY="$API_KEY" \
  REPLICANT_API_SECRET="$API_SECRET" \
  REPLICANT_TEST_USER_ID="$TEST_USER_ID" \
  REPLICANT_TEST_EMAIL="$TEST_EMAIL" \
  REPLICANT_TEST_CURATED_ID="$CURATED_ID" \
  REPLICANT_LEGACY_API_KEY="$LEGACY_API_KEY" \
  REPLICANT_LEGACY_API_SECRET="$LEGACY_API_SECRET" \
  SYNC_SERVER_URL="ws://localhost:$SERVER_PORT/socket/websocket" \
  bash -c "${INTEROP_TEST_CMD:-cargo test -p replicant-client --test interop --test v2_smoke --test two_process -- --include-ignored --skip child_ --test-threads=1 ${1:+\"$1\"}}" )
test_exit=$?
set -e

echo ""
if [ $test_exit -eq 0 ]; then
    log "Interop suite passed"
else
    err "Interop suite failed (server log tail below)"
    tail -40 "$SERVER_LOG"
fi
exit $test_exit
