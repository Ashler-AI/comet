#!/usr/bin/env bash
# Idempotent entry point for Namespace boot and an explicit Crew wake request.
set -euo pipefail
channel="${1:-staging}"
case "$channel" in
  staging) port=27655 ;;
  production) port=27653 ;;
  *) echo 'Expected staging or production.' >&2; exit 1 ;;
esac
wrapper="$HOME/.local/bin/crew-devbox-$channel"
[[ -x "$wrapper" ]] || { echo "Configure Crew $channel on this Devbox first." >&2; exit 1; }
command -v tmux >/dev/null || { echo 'tmux is required for Crew Devbox startup.' >&2; exit 1; }
# Ignore an inherited TMUX socket from Namespace's declared dev-shell. CLI wake
# and boot must address the same server rather than create competing supervisors.
session="crew-engine-$channel"
if ! tmux -L crew-devbox has-session -t "=$session" 2>/dev/null; then
  # A manually started engine must not acquire a competing supervisor.
  if python3 - "$port" <<'PY'
import socket, sys
with socket.socket() as connection:
    connection.settimeout(0.25)
    sys.exit(0 if connection.connect_ex(('127.0.0.1', int(sys.argv[1]))) == 0 else 1)
PY
  then
    exit 0
  fi
  printf -v command 'sleep 5; export ASHLER_INCREMENTAL_TSC_CHECKS=false; exec %q headless' "$wrapper"
  tmux -L crew-devbox new-session -d -s "$session" -c "$HOME" "$command" || tmux -L crew-devbox has-session -t "=$session"
fi
tmux -L crew-devbox set-option -w -t "$session:0" remain-on-exit on
tmux -L crew-devbox set-hook -w -t "$session:0" pane-died "respawn-pane -t $session:0.0"
if [[ "$(tmux -L crew-devbox display-message -p -t "$session:0.0" '#{pane_dead}')" == 1 ]]; then
  tmux -L crew-devbox respawn-pane -t "$session:0.0"
fi
