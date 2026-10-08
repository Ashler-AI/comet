#!/usr/bin/env bash
# One-command demo: boots a seeded engine daemon + the headed app, offline.
# Made for judging look & feel with real input — no edge, no auth needed.
#
#   scripts/dev-demo.sh            # build, seed demo data, open the app
#   scripts/dev-demo.sh --slow     # pace mock streams (~10s) to watch streaming
#
# Everything lives under /tmp/comet-demo-*; re-runs reuse it. Ctrl-C cleans up.
set -euo pipefail
export ASHLER_INCREMENTAL_TSC_CHECKS=false
cd "$(dirname "$0")/.."

DAEMON_DIR=${COMET_DEMO_DAEMON_DIR:-/tmp/comet-demo-daemon}
UI_DIR=${COMET_DEMO_UI_DIR:-/tmp/comet-demo-ui}
IPC=${COMET_DEMO_IPC_PORT:-27921}
DELAY=""
[[ "${1:-}" == "--slow" ]] && DELAY=350

echo "▸ building Crew (first run takes a few minutes)…"
DEMO_BIN="$(mktemp -d)"
trap 'rm -rf "$DEMO_BIN"' EXIT
python3 scripts/local-cargo.py build --locked -p comet -p comet-rpc --bins --example rpc_probe -q \
  --copy-binary debug/comet "$DEMO_BIN/comet" \
  --copy-binary debug/examples/rpc_probe "$DEMO_BIN/rpc_probe"

echo "▸ starting engine daemon on :$IPC"
env COMET_DATA_DIR="$DAEMON_DIR" COMET_IPC_PORT=$IPC COMET_HARNESS=mock \
  COMET_EDGE_TOKEN=demo-token COMET_EDGE_URL=http://127.0.0.1:9 \
  ${DELAY:+COMET_MOCK_DELAY_MS=$DELAY} RUST_LOG=warn \
  "$DEMO_BIN/comet" headless &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null || true; rm -rf "$DEMO_BIN"' EXIT
for _ in $(seq 1 40); do
  (exec 3<>/dev/tcp/127.0.0.1/$IPC) 2>/dev/null && { exec 3>&-; break; }
  sleep 0.25
done

probe() { "$DEMO_BIN/rpc_probe" "ws://127.0.0.1:$IPC" "$@"; }

if [[ ! -f "$DAEMON_DIR/.demo-seeded" ]]; then
  echo "▸ seeding demo chats"
  DEV=$(probe LocalDevice '{}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["deviceId"])')
  # One space per demo folder, created up-front (chats join by space id).
  declare -A SPACES=()
  for project in comet-native soccertcg comet aether; do
    sid=$(uuidgen | tr 'A-Z' 'a-z')
    probe Mutate "{\"op\":\"createSpace\",\"spaceId\":\"$sid\",\"deviceId\":\"$DEV\",\"path\":\"$HOME/github/$project\"}" >/dev/null
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
  seed "Native Crew Rust Rewrite"     comet-native comet-native/main                 0  run
  seed "Rebalance Player Stats Caps"  soccertcg    comet/rebalance-player-stat-caps  2  run
  seed "Craft Premium TCG Experience" soccertcg    comet/craft-premium-tcg-exp       26 skip
  seed "Initial Context Exploration"  comet        comet/initial-context-exploration 14 skip
  seed "Soccer TCG Repo Creation"     aether       aether/main                       48 skip
  touch "$DAEMON_DIR/.demo-seeded"
fi

echo "▸ opening Crew (composer is live — type into it; --slow shows streaming)"
COMET_DATA_DIR="$UI_DIR" COMET_IPC_PORT=$IPC COMET_EDGE_TOKEN=demo-token \
  COMET_EDGE_URL=http://127.0.0.1:9 RUST_LOG=warn "$DEMO_BIN/comet"
