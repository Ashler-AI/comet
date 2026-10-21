#!/usr/bin/env bash
# Idempotent entry point for boot and an explicit Crew devbox wake request.
set -euo pipefail
channel="${1:-staging}"
case "$channel" in
  staging) port=27655 ;;
  production) port=27653 ;;
  *) echo 'Expected staging or production.' >&2; exit 1 ;;
esac
wrapper="$HOME/.local/bin/crew-devbox-$channel"
[[ -x "$wrapper" ]] || { echo "Configure Crew $channel on this Devbox first." >&2; exit 1; }
config="$HOME/.config/crew-devbox/$channel.env"
[[ -r "$config" ]] || { echo 'Configure this Crew channel before starting it.' >&2; exit 1; }
source "$config"
port_listening() {
  python3 - "$1" <<'PY'
import socket, sys
with socket.socket() as connection:
    connection.settimeout(0.25)
    sys.exit(0 if connection.connect_ex(('127.0.0.1', int(sys.argv[1]))) == 0 else 1)
PY
}
other_port=27655
[[ "$channel" == staging ]] && other_port=27653
if port_listening "$other_port"; then
  echo 'The other Crew channel is running. Finish its active turns before switching.' >&2
  exit 1
fi
case "${COMET_DEVBOX_PROVIDER:-}" in
  boat)
    [[ "$(uname -s)" == Linux && "$COMET_DEVBOX_ID" =~ ^bx_[23456789abcdefghjkmnpqrstuvwxyz]{8}$ && "${BOAT_ID:-}" == "$COMET_DEVBOX_ID" ]] || { echo 'The configured Boat ID does not match this machine.' >&2; exit 1; }
    systemctl --user import-environment BOAT_ID
    if port_listening "$port"; then exit 0; fi
    exec "$wrapper" daemon install
    ;;
  namespace)
    [[ "$(uname -s)" == Linux && -d /.namespace/tasks && "$COMET_DEVBOX_ID" =~ ^[a-z0-9]{13}$ && -z "${BOAT_ID:-}" ]] || { echo 'Invalid Namespace host identity.' >&2; exit 1; }
    ;;
  *) echo 'COMET_DEVBOX_PROVIDER must be namespace or boat.' >&2; exit 1 ;;
esac
command -v tmux >/dev/null || { echo 'tmux is required for Crew Devbox startup.' >&2; exit 1; }
# Ignore an inherited TMUX socket from Namespace's declared dev-shell. CLI wake
# and boot must address the same server rather than create competing supervisors.
session="crew-engine-$channel"
if ! tmux -L crew-devbox has-session -t "=$session" 2>/dev/null; then
  # A manually started engine must not acquire a competing supervisor.
  if port_listening "$port"; then
    exit 0
  fi
  printf -v command 'sleep 5; export ASHLER_INCREMENTAL_TSC_CHECKS=false; exec %q headless' "$wrapper"
  tmux -L crew-devbox new-session -d -s "$session" -n engine -c "$HOME" "$command" || tmux -L crew-devbox has-session -t "=$session"
fi
window_id="$(tmux -L crew-devbox list-windows -t "=$session" -F '#{window_id}')"
pane_id="$(tmux -L crew-devbox list-panes -t "$window_id" -F '#{pane_id}')"
[[ -n "$window_id" && -n "$pane_id" ]] || { echo "Crew $channel tmux session has no pane." >&2; exit 1; }
tmux -L crew-devbox set-option -w -t "$window_id" remain-on-exit on
tmux -L crew-devbox set-hook -w -t "$window_id" pane-died "respawn-pane -t $pane_id"
if [[ "$(tmux -L crew-devbox display-message -p -t "$pane_id" '#{pane_dead}')" == 1 ]]; then
  tmux -L crew-devbox respawn-pane -t "$pane_id"
fi
