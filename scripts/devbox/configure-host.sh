#!/usr/bin/env bash
# Install staging launchers on an explicitly identified existing host; never provision or sign in.
set -euo pipefail
: "${COMET_DEVBOX_PROVIDER:?Set namespace or boat}"
: "${COMET_DEVBOX_ID:?Set this host's immutable provider ID}"
: "${CREW_BINARY:?Set the absolute path to the verified staging Crew executable}"
: "${COMET_DEVICE_NAME:?Set the owner-qualified device display name}"
[[ "$(uname -s)" == Linux ]] || { echo 'Run on the target Linux host.' >&2; exit 1; }
[[ "$CREW_BINARY" == /* && -x "$CREW_BINARY" ]] || { echo 'CREW_BINARY must be an absolute executable path.' >&2; exit 1; }
case "$COMET_DEVBOX_PROVIDER" in
  namespace)
    [[ -d /.namespace/tasks && "$COMET_DEVBOX_ID" =~ ^[a-z0-9]{13}$ && -z "${BOAT_ID:-}" ]] || { echo 'Invalid Namespace host identity.' >&2; exit 1; }
    command -v tmux >/dev/null
    ;;
  boat)
    [[ "$COMET_DEVBOX_ID" =~ ^bx_[23456789abcdefghjkmnpqrstuvwxyz]{8}$ && "${BOAT_ID:-}" == "$COMET_DEVBOX_ID" && ! -d /.namespace/tasks ]] || { echo 'Boat ID does not match this host.' >&2; exit 1; }
    command -v boat >/dev/null
    ;;
  *) echo 'Expected namespace or boat.' >&2; exit 1 ;;
esac
command -v python3 >/dev/null
mkdir -p "$HOME/.config/crew-devbox" "$HOME/.local/bin"
config="$HOME/.config/crew-devbox/staging.env"
if [[ -f "$config" ]]; then
  old_provider="$COMET_DEVBOX_PROVIDER" old_id="$COMET_DEVBOX_ID"
  (source "$config"; [[ "$COMET_DEVBOX_PROVIDER" == "$old_provider" && "$COMET_DEVBOX_ID" == "$old_id" ]]) || {
    echo 'Existing host binding differs; migrate the profile explicitly before reconfiguration.' >&2; exit 1;
  }
fi
(umask 077
  printf 'export COMET_DEVICE_NAME=%q\n' "$COMET_DEVICE_NAME"
  printf 'export COMET_DATA_DIR=%q\n' "$HOME/.comet-native-staging"
  printf 'export COMET_EDGE_URL=%q\n' 'https://comet-staging.internal.ashler.com'
  printf 'export COMET_SCAFFOLD_URL=%q\n' 'https://scaffold-staging.internal.ashler.com'
  printf 'export COMET_PROJECT_SCOPE=%q\n' 'ashler-staging'
  printf 'export COMET_DEPLOYMENT_ID=%q\n' 'ashler-staging'
  printf 'export COMET_IPC_PORT=27655\n'
  printf 'export COMET_DEVBOX_PROVIDER=%q\n' "$COMET_DEVBOX_PROVIDER"
  printf 'export COMET_DEVBOX_ID=%q\n' "$COMET_DEVBOX_ID"
  printf 'export PATH=%q\n' "$HOME/.local/bin:$PATH"
) > "$config"
chmod 600 "$config"
printf '#!/usr/bin/env bash\nset -euo pipefail\nsource %q\nexec %q "$@"\n' "$config" "$CREW_BINARY" > "$HOME/.local/bin/crew-devbox-staging"
chmod 755 "$HOME/.local/bin/crew-devbox-staging"
install -m 755 "$(dirname "${BASH_SOURCE[0]}")/autostart.sh" "$HOME/.local/bin/crew-devbox-autostart"
install -m 755 "$(dirname "${BASH_SOURCE[0]}")/keepalive.py" "$HOME/.local/bin/crew-devbox-keepalive"
if [[ "$COMET_DEVBOX_PROVIDER" == namespace ]]; then
  python3 - <<'PY'
from pathlib import Path
path = Path.home() / '.bashrc'
old = path.read_text() if path.exists() else ''
first = '# >>> Crew Devbox staging autostart >>>'
last = '# <<< Crew Devbox staging autostart <<<'
if first in old or last in old:
    if old.count(first) != 1 or old.count(last) != 1 or old.index(first) > old.index(last):
        raise SystemExit('Malformed staging autostart block; preserve and repair it explicitly.')
    before, rest = old.split(first, 1)
    _, after = rest.split(last, 1)
    old = before + after
path.write_text(old.rstrip() + '\n\n' + first + '\n"$HOME/.local/bin/crew-devbox-autostart" staging\n' + last + '\n')
PY
fi
printf 'Configured staging. No engine restarted and no credentials copied.\nSign in interactively: %s login\nStart: %s staging\n' "$HOME/.local/bin/crew-devbox-staging" "$HOME/.local/bin/crew-devbox-autostart"
