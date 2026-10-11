#!/usr/bin/env bash
set -euo pipefail
autostart="${1:-$(dirname "$0")/../skills/devbox/scripts/autostart.sh}"
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
mkdir -p "$HOME/.local/bin" "$HOME/.config/crew-devbox" "$root/bin"
# No real engine or listening port: exercise tmux ownership, not Crew startup.
printf '#!/bin/sh\nexit 1\n' > "$root/bin/python3"
printf '#!/bin/sh\nexec sleep 60\n' > "$HOME/.local/bin/crew-devbox-staging"
chmod +x "$root/bin/python3" "$HOME/.local/bin/crew-devbox-staging"
printf 'export COMET_DEVBOX_PROVIDER=namespace\nexport COMET_DEVBOX_ID=ofpf7g22n4412\n' > "$HOME/.config/crew-devbox/staging.env"
export PATH="$root/bin:$PATH"
tmux -L namespace new-session -d -s dev-shell 'sleep 60'
export TMUX="$(tmux -L namespace display-message -p '#{socket_path}'),0,0"
bash "$autostart" staging
before=$(tmux -L crew-devbox list-panes -t '=crew-engine-staging' -F '#{pane_pid}')
unset TMUX
bash "$autostart" staging
after=$(tmux -L crew-devbox list-panes -t '=crew-engine-staging' -F '#{pane_pid}')
[[ "$before" == "$after" ]]
printf 'Boot and CLI wake reuse one supervisor despite inherited TMUX.\n'
