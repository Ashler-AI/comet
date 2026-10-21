#!/usr/bin/env bash
# Two-device e2e smoke: real edge (wrangler dev), two headless engines, and the
# zeron-rpc e2e_driver example proving the doc-queued cross-device command path:
#
#   B queues a Run into the chat doc -> nudge -> A (host) executes via the mock
#   harness -> transcript + session status sync A -> edge -> B.
#
# Both engines run as the SAME user (alice@org1) on different devices — zeron's
# one-user-many-devices model; chat/device rooms are claim-on-first-join per user.
#
# Usage: scripts/e2e-smoke.sh
# Env: ZERON_E2E_EDGE_PORT (otherwise a free port), ZERON_E2E_KEEP_LOGS=1.

set -euo pipefail
export ASHLER_INCREMENTAL_TSC_CHECKS=false

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
ORG="crew-e2e"
TOKEN="alice@$ORG"
LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/crew-e2e.XXXXXX")"
A_DIR="$LOG_DIR/a"
B_DIR="$LOG_DIR/b"
A_PORT=""
B_PORT=""

EDGE_PID=""
A_PID=""
B_PID=""
STATUS=1

cleanup() {
  for pid in "$A_PID" "$B_PID"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
    fi
  done
  # The edge runs in its own process group — kill the whole wrangler group
  # (npx → wrangler → workerd children).
  [[ -n "$EDGE_PID" ]] && kill -- -"$EDGE_PID" 2>/dev/null || true
  sleep 1
  for pid in "$A_PID" "$B_PID"; do
    [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null || true
  done
  [[ -n "$EDGE_PID" ]] && kill -9 -- -"$EDGE_PID" 2>/dev/null || true
  rm -rf "$A_DIR" "$B_DIR"
  if [[ "$STATUS" -ne 0 ]]; then
    echo "--- engine A log (tail) ---"; tail -n 40 "$LOG_DIR/engine-a.log" 2>/dev/null || true
    echo "--- engine B log (tail) ---"; tail -n 40 "$LOG_DIR/engine-b.log" 2>/dev/null || true
    echo "--- edge log (tail) ---"; tail -n 40 "$LOG_DIR/edge.log" 2>/dev/null || true
  fi
  if [[ "${ZERON_E2E_KEEP_LOGS:-0}" != "1" ]]; then
    rm -rf "$LOG_DIR"
  else
    echo "logs kept in $LOG_DIR"
  fi
}
trap cleanup EXIT

wait_for() { # wait_for <description> <timeout_s> <command...>
  local what="$1" timeout="$2"; shift 2
  local waited=0
  until "$@" >/dev/null 2>&1; do
    sleep 1
    waited=$((waited + 1))
    if [[ "$waited" -ge "$timeout" ]]; then
      echo "FAIL: timed out waiting for $what" >&2
      exit 1
    fi
  done
}

# Bind all three ports together before choosing them; an explicit occupied Edge
# port fails instead of reusing another session's worker or persisted rooms.
read -r EDGE_PORT A_PORT B_PORT < <(node --input-type=module -e '
  import { createServer } from "node:net";
  const servers = await Promise.all([process.env.ZERON_E2E_EDGE_PORT || 0, 0, 0].map(port =>
    new Promise((resolve, reject) => {
      const server = createServer().once("error", reject);
      server.listen(Number(port), "127.0.0.1", () => resolve(server));
    })));
  console.log(servers.map(server => server.address().port).join(" "));
  servers.forEach(server => server.close());
')
EDGE_URL="http://127.0.0.1:$EDGE_PORT"

# ── 1. Fresh local-only Edge, with explicit development authority ─────────────
echo "edge: starting isolated local worker on :$EDGE_PORT"
set -m
(
  cd "$ROOT/edge"
  exec node_modules/.bin/wrangler dev --env local --local --ip 127.0.0.1 \
    --port "$EDGE_PORT" --persist-to "$LOG_DIR/edge" \
    --var AUTH_MODE:dev --var ENVIRONMENT:local \
    --var "SCAFFOLD_PROJECT_SCOPE:$ORG" --var "SCAFFOLD_DEPLOYMENT_ID:$ORG" \
    --var "SCAFFOLD_CONTROL_PLANE_URL:$EDGE_URL"
) >"$LOG_DIR/edge.log" 2>&1 &
EDGE_PID=$!
set +m
wait_for "edge /health" 90 curl -sf -m 3 "$EDGE_URL/health"

# ── 2. Build the binaries (workspace target is warm in CI/dev) ─────────────────
echo "build: zeron + e2e_driver"
# Two invocations: a single one with `--example` builds ONLY the example
# (the target filter applies across every -p), silently skipping the zeron
# bin — the smoke then dies on "No such file or directory".
(cd "$ROOT" && cargo build -q -p zeron)
(cd "$ROOT" && cargo build -q -p zeron-rpc --example e2e_driver)
ZERON="$ROOT/target/debug/zeron"
DRIVER="$ROOT/target/debug/examples/e2e_driver"

# ── 3. Two headless engines, one user, two devices ─────────────────────────────
mkdir -p "$A_DIR" "$B_DIR" "$LOG_DIR/home"

start_engine() { # start_engine <data_dir> <ipc_port> <name> <log>
  env -i HOME="$LOG_DIR/home" PATH="$PATH" TMPDIR="${TMPDIR:-/tmp}" \
    ASHLER_INCREMENTAL_TSC_CHECKS=false ZERON_NO_LOGIN_SHELL=1 \
    ZERON_DATA_DIR="$1" ZERON_IPC_PORT="$2" ZERON_DEVICE_NAME="$3" \
    ZERON_EDGE_URL="$EDGE_URL" ZERON_EDGE_TOKEN="$TOKEN" ZERON_ORG_ID="$ORG" \
    ZERON_HARNESS=mock RUST_LOG=info "$ZERON" headless >"$4" 2>&1 &
}

start_engine "$A_DIR" "$A_PORT" "e2e-device-a" "$LOG_DIR/engine-a.log"; A_PID=$!
start_engine "$B_DIR" "$B_PORT" "e2e-device-b" "$LOG_DIR/engine-b.log"; B_PID=$!

wait_for "engine A ipc :$A_PORT" 60 bash -c "exec 3<>/dev/tcp/127.0.0.1/$A_PORT"
wait_for "engine B ipc :$B_PORT" 60 bash -c "exec 3<>/dev/tcp/127.0.0.1/$B_PORT"
echo "engines: A pid=$A_PID ipc=:$A_PORT  B pid=$B_PID ipc=:$B_PORT"

# ── 4. Drive the cross-device flow through both IPCs ───────────────────────────
"$DRIVER" "$A_PORT" "$B_PORT"
STATUS=$?
exit "$STATUS"
