#!/usr/bin/env bash
# Run inside the owner's Namespace or Boat devbox with a verified Crew release.
set -euo pipefail
: "${CREW_OWNER_NAME:?Set the real owner display name}"
: "${COMET_DEVBOX_PROVIDER:?Set namespace or boat}"
: "${CREW_DEVBOX_NAME:?Set the owner-qualified devbox name}"
: "${COMET_DEVBOX_ID:?Set the immutable provider ID}"
: "${CREW_BINARY:?Set the absolute path to the verified Crew executable}"
CREW_CHANNEL="${CREW_CHANNEL:-production}"
[[ "$(uname -s)" == Linux ]] || { echo 'Run inside a Linux devbox.' >&2; exit 1; }
case "$COMET_DEVBOX_PROVIDER" in
  namespace)
    [[ -d /.namespace/tasks && "$COMET_DEVBOX_ID" =~ ^[a-z0-9]{13}$ ]] || { echo 'Invalid Namespace host or immutable ID.' >&2; exit 1; }
    CREW_FOLDER_ROOT="${CREW_FOLDER_ROOT:-/workspaces}"
    ;;
  boat)
    [[ "$COMET_DEVBOX_ID" =~ ^bx_[23456789abcdefghjkmnpqrstuvwxyz]{8}$ && "${BOAT_ID:-}" == "$COMET_DEVBOX_ID" ]] || { echo 'The configured Boat ID must match this machine.' >&2; exit 1; }
    CREW_FOLDER_ROOT="${CREW_FOLDER_ROOT:-$HOME}"
    export ASHLER_LOCALDEV_DATA_DIR="${ASHLER_LOCALDEV_DATA_DIR:-$HOME/.local/share/ashler-localdev}"
    [[ "$ASHLER_LOCALDEV_DATA_DIR" == /* ]] || { echo 'ASHLER_LOCALDEV_DATA_DIR must be an absolute path.' >&2; exit 1; }
    command -v boat >/dev/null || { echo 'boat is required for sleep protection.' >&2; exit 1; }
    ;;
  *) echo 'COMET_DEVBOX_PROVIDER must be namespace or boat.' >&2; exit 1 ;;
esac
[[ "$CREW_BINARY" == /* && -x "$CREW_BINARY" ]] || { echo 'CREW_BINARY must be an absolute executable path.' >&2; exit 1; }
[[ "$CREW_DEVBOX_NAME" =~ ^[a-zA-Z0-9][a-zA-Z0-9._-]*$ ]] || { echo 'Invalid devbox name.' >&2; exit 1; }
[[ "$CREW_OWNER_NAME" != *$'\n'* && -n "${CREW_OWNER_NAME// /}" ]] || { echo 'Invalid owner name.' >&2; exit 1; }
[[ "$CREW_FOLDER_ROOT" == /* && -d "$CREW_FOLDER_ROOT" ]] || { echo 'CREW_FOLDER_ROOT must be an accessible absolute directory.' >&2; exit 1; }
case "$CREW_CHANNEL" in
  staging) edge=https://comet-staging.internal.ashler.com; scaffold=https://scaffold-staging.internal.ashler.com; scope=ashler-staging; data="$HOME/.comet-native-staging"; port=27655; callback_port=27656 ;;
  production) edge=https://comet.internal.ashler.com; scaffold=https://scaffold.internal.ashler.com; scope=ashler-production; data="$HOME/.comet-native"; port=27653; callback_port=27654 ;;
  *) echo 'CREW_CHANNEL must be staging or production.' >&2; exit 1 ;;
esac
for tool in node npm python3 omp gcloud; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }; done
if [[ "${CREW_DEVBOX_SETUP_PROTECTED:-}" != 1 ]]; then
  exec python3 "$(dirname "${BASH_SOURCE[0]}")/keepalive.py" env CREW_DEVBOX_SETUP_PROTECTED=1 bash "$0" "$@"
fi
browser_home="$HOME/.local/share/crew-devbox-browser"
mkdir -p "$browser_home" "$HOME/.local/bin" "$HOME/.config/crew-devbox"
npm install --prefix "$browser_home" --no-audit --no-fund --save-exact playwright@1.57.0
export PLAYWRIGHT_BROWSERS_PATH="$browser_home/browsers"
browser="$(node -e 'process.stdout.write(require(process.argv[1]).chromium.executablePath())' "$browser_home/node_modules/playwright")"
if [[ ! -x "$browser" ]]; then
  node "$browser_home/node_modules/playwright/cli.js" install --with-deps chromium
fi
[[ -x "$browser" ]] || { echo 'Chromium installation did not produce an executable.' >&2; exit 1; }
printf '#!/usr/bin/env bash\nexec %q "$@"\n' "$browser" > "$HOME/.local/bin/crew-chromium"
chmod 755 "$HOME/.local/bin/crew-chromium"

config="$HOME/.config/crew-devbox/$CREW_CHANNEL.env"
(umask 077
  printf 'export COMET_DEVICE_NAME=%q\n' "$CREW_OWNER_NAME · Devbox · $CREW_DEVBOX_NAME"
  printf 'export COMET_DATA_DIR=%q\n' "$data"
  printf 'export COMET_EDGE_URL=%q\n' "$edge"
  printf 'export COMET_SCAFFOLD_URL=%q\n' "$scaffold"
  printf 'export PATH=%q\n' "$HOME/.local/bin:$PATH"
  printf 'export COMET_PROJECT_SCOPE=%q\n' "$scope"
  printf 'export COMET_IPC_PORT=%q\n' "$port"
  printf 'export COMET_CALLBACK_PORT=%q\n' "$callback_port"
  printf 'export COMET_DEFAULT_FOLDER=%q\n' "$CREW_FOLDER_ROOT"
  printf 'export ASHLER_INCREMENTAL_TSC_CHECKS=false\n'
  printf 'export CHROME_PATH=%q\n' "$browser"
  printf 'export PUPPETEER_EXECUTABLE_PATH=%q\n' "$browser"
  printf 'export PLAYWRIGHT_BROWSERS_PATH=%q\n' "$PLAYWRIGHT_BROWSERS_PATH"
  printf 'export COMET_DEVBOX_PROVIDER=%q\n' "$COMET_DEVBOX_PROVIDER"
  printf 'export COMET_DEVBOX_ID=%q\n' "$COMET_DEVBOX_ID"
  if [[ "$COMET_DEVBOX_PROVIDER" == boat ]]; then
    printf 'export ASHLER_LOCALDEV_DATA_DIR=%q\n' "$ASHLER_LOCALDEV_DATA_DIR"
  fi
) > "$config"
chmod 600 "$config"
printf '#!/usr/bin/env bash\nset -euo pipefail\nsource %q\nexec %q "$@"\n' "$config" "$CREW_BINARY" > "$HOME/.local/bin/crew-devbox-$CREW_CHANNEL"
chmod 755 "$HOME/.local/bin/crew-devbox-$CREW_CHANNEL"
install -m 755 "$(dirname "${BASH_SOURCE[0]}")/autostart.sh" "$HOME/.local/bin/crew-devbox-autostart"
install -m 755 "$(dirname "${BASH_SOURCE[0]}")/keepalive.py" "$HOME/.local/bin/crew-devbox-keepalive"
cat > "$HOME/.local/bin/crew-devbox-gcloud-login" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
command -v gcloud >/dev/null || { echo 'gcloud is required for gcloud login.' >&2; exit 1; }
export DISPLAY=:99
export BROWSER=/bin/echo
exec gcloud auth login --force "$@"
SH
chmod 755 "$HOME/.local/bin/crew-devbox-gcloud-login"

# Preserve personal guidance; migrate only this setup's bounded managed block.
python3 - "$CREW_DEVBOX_NAME" "$CREW_CHANNEL" "$COMET_DEVBOX_PROVIDER" "${ASHLER_LOCALDEV_DATA_DIR:-}" <<'PY'
from pathlib import Path
from shlex import quote
import sys
name, channel, provider, datastore_dir = sys.argv[1:]
start, end = '<!-- crew-devbox:start -->', '<!-- crew-devbox:end -->'
blocks = [(start, end), ('<!-- crew-namespace-devbox:start -->', '<!-- crew-namespace-devbox:end -->')]
block = f'''{start}
## Devbox execution
You are running on {provider} devbox `{name}`, not the user's Mac.
- Credentials must be created and stored on this devbox. Preserve interactive browser approval and MFA; never copy credential directories or use gcloud remote bootstrap.
- Setup owns the short-lived callback tunnels. For gcloud reauthentication use `~/.local/bin/crew-devbox-gcloud-login`; if localhost:8085 is not forwarded, rerun the devbox skill's authentication phase. Do not start a second login while the existing flow is waiting.
- Never print tokens, keys, cookies, callback codes, or secret files. Use Crew sign-in, Crew Agent Auth, and Infisical localdev rendering, not production credentials or separate model-provider logins.
- Chromium is `~/.local/bin/crew-chromium`. Keep CDP, Tilt and credential callbacks loopback-only. The web app must retain its maintained Docker-reachable binding for pod callbacks through `host.docker.internal:3000`; keep public inbound app access denied by provider private access or the host firewall. Never publish these services through provider hosting.
- Crew protects executing turns, not parked harnesses. For independent work that outlives a turn, use `~/.local/bin/crew-devbox-keepalive <command> [arguments...]`. Namespace uses owned task markers; Boat renews a finite lease and stops the command if renewal fails. Inspect partial work before retrying.
- A crash can leave a Namespace marker behind. Remove only confirmed abandoned owned markers; never clear all tasks. Boat stop does not run ordinary process cleanup, and resume restarts processes rather than restoring memory.
- Keep `ASHLER_INCREMENTAL_TSC_CHECKS=false`; do not run local typechecks. Use Docker + kind + Tilt, not OrbStack. Never reset or seed a shared or production database.
{end}'''
for relative in ('.omp/agent/AGENTS.md', '.claude/CLAUDE.md', '.codex/AGENTS.md'):
    path = Path.home() / relative
    old = path.read_text() if path.exists() else ''
    for first, last in blocks:
        if first in old or last in old:
            if old.count(first) != 1 or old.count(last) != 1 or old.index(first) > old.index(last):
                raise SystemExit(f'Malformed managed guidance in {path}')
            before, rest = old.split(first, 1)
            _, after = rest.split(last, 1)
            old = before + after
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(old.rstrip() + '\n\n' + block + '\n')

# Namespace runs its declared dev-shell at boot. Boat uses one systemd unit.
path = Path.home() / '.bashrc'
old = path.read_text() if path.exists() else ''
managed = [
    ('# >>> Crew Devbox staging autostart >>>', '# <<< Crew Devbox staging autostart <<<'),
    ('# >>> Crew Devbox production autostart >>>', '# <<< Crew Devbox production autostart <<<'),
    ('# >>> Crew Devbox autostart >>>', '# <<< Crew Devbox autostart <<<'),
]
for first, last in managed:
    if first in old or last in old:
        if old.count(first) != 1 or old.count(last) != 1 or old.index(first) > old.index(last):
            raise SystemExit(f'Malformed Crew startup block in {path}')
        before, rest = old.split(first, 1)
        _, after = rest.split(last, 1)
        old = before + after
if provider == 'namespace':
    first, last = f'# >>> Crew Devbox {channel} autostart >>>', f'# <<< Crew Devbox {channel} autostart <<<'
    old = old.rstrip() + f'\n\n{first}\n"$HOME/.local/bin/crew-devbox-autostart" {channel}\n{last}\n'
elif provider == 'boat':
    first, last = f'# >>> Crew Devbox {channel} autostart >>>', f'# <<< Crew Devbox {channel} autostart <<<'
    old = f'{first}\nexport ASHLER_LOCALDEV_DATA_DIR={quote(datastore_dir)}\n{last}\n\n' + old
path.write_text(old)
PY
"$HOME/.local/bin/crew-chromium" --version
printf 'Configured %s for %s. Existing engines were NOT restarted.\n' "$CREW_CHANNEL" "$CREW_OWNER_NAME"
printf 'Sign in: %s login\nStart: %s %s\n' "$HOME/.local/bin/crew-devbox-$CREW_CHANNEL" "$HOME/.local/bin/crew-devbox-autostart" "$CREW_CHANNEL"
