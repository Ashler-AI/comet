#!/usr/bin/env bash
# One-command demo: boots a seeded engine daemon + the headed app, offline.
# Made for judging look & feel with real input — no edge, no auth needed.
#
#   scripts/dev-demo.sh            # build, seed demo data, open the app
#   scripts/dev-demo.sh --slow     # pace mock streams (~10s) to watch streaming
#
# Each run owns a fresh crew-demo-* directory; Ctrl-C cleans it up.
set -euo pipefail
export ASHLER_INCREMENTAL_TSC_CHECKS=false
cd "$(dirname "$0")/.."

DEMO_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/crew-demo.XXXXXX")"
DAEMON_DIR="$DEMO_ROOT/daemon"
UI_DIR="$DEMO_ROOT/ui"
PROJECTS_DIR="$DEMO_ROOT/projects"
IPC=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
DELAY=""
[[ "${1:-}" == "--slow" ]] && DELAY=350

echo "▸ building (first run takes a few minutes)…"
# This is a source/mock launch, never a packaged application identity.
unset COMET_PACKAGE_ENVIRONMENT COMET_DEFAULT_ENVIRONMENT
CREW="$DEMO_ROOT/comet"
PROBE="$DEMO_ROOT/rpc_probe"
trap 'rm -rf "$DEMO_ROOT"' EXIT
python3 scripts/local-cargo.py build --locked -p zeron -p zeron-rpc --bin zeron --example rpc_probe \
  --copy-binary debug/zeron "$CREW" --copy-binary debug/examples/rpc_probe "$PROBE"

echo "▸ starting engine daemon on :$IPC"
env -i PATH="$PATH" HOME="$HOME" TMPDIR="${TMPDIR:-/tmp}" \
  ASHLER_INCREMENTAL_TSC_CHECKS=false COMET_HARNESS=mock \
  ${DELAY:+ZERON_MOCK_DELAY_MS=$DELAY} RUST_LOG=warn \
  "$CREW" --data-dir "$DAEMON_DIR" --ipc-port "$IPC" \
  --edge-url http://127.0.0.1:27941 headless &
DAEMON_PID=$!
trap 'kill "$DAEMON_PID" 2>/dev/null || true; rm -rf "$DEMO_ROOT"' EXIT
for _ in $(seq 1 40); do
  kill -0 "$DAEMON_PID" 2>/dev/null || { wait "$DAEMON_PID"; exit 1; }
  (exec 3<>/dev/tcp/127.0.0.1/$IPC) 2>/dev/null && { exec 3>&-; break; }
  sleep 0.25
done
kill -0 "$DAEMON_PID" 2>/dev/null || { wait "$DAEMON_PID"; exit 1; }

probe() { ZERON_DATA_DIR="$DAEMON_DIR" "$PROBE" "ws://127.0.0.1:$IPC" "$@"; }

if [[ ! -f "$DAEMON_DIR/.demo-seeded" ]]; then
  echo "▸ seeding demo chats"
  DEV=$(probe LocalDevice '{}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["deviceId"])')
  # One space per demo folder, created up-front (chats join by space id);
  # a folder already registered keeps its first space, so each is listed once.
  declare -A SPACES=()
  for project in zeron soccertcg aether; do
    sid=$(uuidgen | tr 'A-Z' 'a-z')
    mkdir -p "$PROJECTS_DIR/$project"
    probe Mutate "{\"op\":\"createSpace\",\"spaceId\":\"$sid\",\"deviceId\":\"$DEV\",\"path\":\"$PROJECTS_DIR/$project\"}" >/dev/null
    SPACES[$project]="$sid"
  done
  seed() { # title project branch age_hours run
    local id; id=$(uuidgen | tr 'A-Z' 'a-z')
    local sid="${SPACES[$2]}"
    probe Mutate "{\"op\":\"createChat\",\"chatId\":\"$id\",\"spaceId\":\"$sid\",\"config\":{\"harness\":\"mock\",\"model\":\"fable-5\",\"reasoning\":null,\"sandbox\":\"workspace-write\"}}" >/dev/null
    probe Mutate "{\"op\":\"renameChat\",\"chatId\":\"$id\",\"title\":\"$1\"}" >/dev/null
    probe Mutate "{\"op\":\"setChatBranch\",\"chatId\":\"$id\",\"branch\":\"$3\"}" >/dev/null
    if [[ "$5" == run ]]; then
      probe QueueCommand "{\"chatId\":\"$id\",\"command\":{\"kind\":\"run\",\"messageId\":\"$(uuidgen)\",\"request\":{\"prompt\":\"Walk me through the streaming pipeline\",\"model\":null,\"reasoning\":null,\"modelOptions\":{},\"cwd\":\"/tmp\",\"sandbox\":\"workspace-write\",\"autoApprove\":true,\"resume\":null}}}" >/dev/null
      sleep 1
    fi
    probe Mutate "{\"op\":\"setChatActivity\",\"chatId\":\"$id\",\"lastMessageAt\":$(( ($(date +%s) - $4*3600) * 1000 ))}" >/dev/null
  }
  seed "Native Crew Rust Rewrite"     zeron zeron/main                 0  run
  seed "Rebalance Player Stats Caps"  soccertcg    zeron/rebalance-player-stat-caps  2  run
  seed "Craft Premium TCG Experience" soccertcg    zeron/craft-premium-tcg-exp       26 skip
  seed "Initial Context Exploration"  zeron        zeron/initial-context-exploration 14 skip
  seed "Soccer TCG Repo Creation"     aether       aether/main                       48 skip
  touch "$DAEMON_DIR/.demo-seeded"
fi

echo "▸ opening Crew (composer is live — type into it; --slow shows streaming)"
env -i PATH="$PATH" HOME="$HOME" TMPDIR="${TMPDIR:-/tmp}" \
  ASHLER_INCREMENTAL_TSC_CHECKS=false COMET_HARNESS=mock ZERON_IPC_TOKEN_FILE="$DAEMON_DIR/ipc-token" RUST_LOG=warn \
  "$CREW" --data-dir "$UI_DIR" --ipc-port "$IPC" \
  --edge-url http://127.0.0.1:27941
