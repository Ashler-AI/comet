#!/usr/bin/env bash
set -euo pipefail
autostart="${1:-$(dirname "$0")/../skills/namespace-devbox/scripts/autostart.sh}"
root=$(mktemp -d)
export TMUX_TMPDIR="$root" HOME="$root/home"
unset TMUX
cleanup() {
  for server in crew-devbox namespace default; do
    tmux -L "$server" kill-server 2>/dev/null || true
  done
  rm -rf "$root"
}
trap cleanup EXIT
mkdir -p "$HOME/.local/bin" "$root/bin"
# No real engine or listening port: exercise tmux ownership, not Crew startup.
printf '#!/bin/sh\nexit 1\n' > "$root/bin/python3"
printf '#!/bin/sh\nexec sleep 60\n' > "$HOME/.local/bin/crew-devbox-staging"
chmod +x "$root/bin/python3" "$HOME/.local/bin/crew-devbox-staging"
export PATH="$root/bin:$PATH"
tmux -L namespace new-session -d -s dev-shell 'sleep 60'
export TMUX="$(tmux -L namespace display-message -p '#{socket_path}'),0,0"
bash "$autostart" staging
before=$(tmux -L crew-devbox display-message -p -t crew-engine-staging:0.0 '#{pane_pid}')
unset TMUX
bash "$autostart" staging
after=$(tmux -L crew-devbox display-message -p -t crew-engine-staging:0.0 '#{pane_pid}')
[[ "$before" == "$after" ]]
printf 'Boot and CLI wake reuse one supervisor despite inherited TMUX.\n'
