#!/usr/bin/env bash
# Two-device e2e smoke: real edge (wrangler dev), two headless engines, and the
# comet-rpc e2e_driver example proving the doc-queued cross-device command path:
#
#   B queues a Run into the chat doc -> nudge -> A (host) executes via the mock
#   harness -> transcript + session status sync A -> edge -> B.
#
# Both engines run as the SAME user (alice@org1) on different devices — comet's
# one-user-many-devices model; chat/device rooms are claim-on-first-join per user.
#
# Usage: scripts/e2e-smoke.sh
# Env:   COMET_E2E_EDGE_PORT (default 26640), COMET_E2E_KEEP_LOGS=1 to keep logs.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
EDGE_PORT="${COMET_E2E_EDGE_PORT:-26640}"
EDGE_URL="http://localhost:${EDGE_PORT}"
TOKEN="alice@org1"
ORG="org1"
A_PORT=27801
B_PORT=27802
A_DIR=/tmp/e2e-a
B_DIR=/tmp/e2e-b
LOG_DIR="$(mktemp -d /tmp/comet-e2e-logs.XXXXXX)"

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
  # The edge runs in its own session (setsid) — kill the whole wrangler group
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
  if [[ "${COMET_E2E_KEEP_LOGS:-0}" != "1" ]]; then
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

# ── 1. Edge worker (wrangler dev, dev auth: bearer == user@org) ────────────────
if curl -sf -m 3 "$EDGE_URL/health" | grep -q '"auth":"dev"'; then
  echo "edge: reusing healthy dev-mode worker on :$EDGE_PORT"
else
  echo "edge: starting wrangler dev on :$EDGE_PORT"
  setsid bash -c "cd '$ROOT/edge' && exec npx wrangler dev --port '$EDGE_PORT' --var AUTH_MODE:dev" \
    >"$LOG_DIR/edge.log" 2>&1 &
  EDGE_PID=$!
  wait_for "edge /health" 90 curl -sf -m 3 "$EDGE_URL/health"
fi

# ── 2. Build the binaries (workspace target is warm in CI/dev) ─────────────────
echo "build: comet + e2e_driver"
(cd "$ROOT" && cargo build -q -p comet -p comet-rpc --example e2e_driver)
COMET="$ROOT/target/debug/comet"
DRIVER="$ROOT/target/debug/examples/e2e_driver"

# ── 3. Two headless engines, one user, two devices ─────────────────────────────
rm -rf "$A_DIR" "$B_DIR"
mkdir -p "$A_DIR" "$B_DIR"

start_engine() { # start_engine <data_dir> <ipc_port> <name> <log>
  COMET_DATA_DIR="$1" COMET_IPC_PORT="$2" COMET_DEVICE_NAME="$3" \
    COMET_EDGE_URL="$EDGE_URL" COMET_EDGE_TOKEN="$TOKEN" COMET_ORG_ID="$ORG" \
    COMET_HARNESS=mock RUST_LOG=info \
    "$COMET" headless >"$4" 2>&1 &
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
