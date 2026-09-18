#!/usr/bin/env bash
# Run inside the owner's Namespace Devbox after installing a verified Crew release.
set -euo pipefail
: "${CREW_OWNER_NAME:?Set the real owner display name}"
: "${NAMESPACE_DEVBOX_NAME:?Set the Namespace Devbox name}"
: "${CREW_BINARY:?Set the absolute path to the verified Crew executable}"
CREW_CHANNEL="${CREW_CHANNEL:-staging}"
[[ "$(uname -s)" == Linux && -d /.namespace/tasks ]] || { echo 'Run inside a Namespace Linux Devbox.' >&2; exit 1; }
[[ "$CREW_BINARY" == /* && -x "$CREW_BINARY" ]] || { echo 'CREW_BINARY must be an absolute executable path.' >&2; exit 1; }
[[ "$NAMESPACE_DEVBOX_NAME" =~ ^[a-zA-Z0-9][a-zA-Z0-9._-]*$ ]] || { echo 'Invalid Devbox name.' >&2; exit 1; }
[[ "$CREW_OWNER_NAME" != *$'\n'* && -n "${CREW_OWNER_NAME// /}" ]] || { echo 'Invalid owner name.' >&2; exit 1; }
case "$CREW_CHANNEL" in
  staging) edge=https://comet-staging.internal.ashler.com; scaffold=https://scaffold-staging.internal.ashler.com; scope=ashler-staging; data="$HOME/.comet-native-staging"; port=27655 ;;
  production) edge=https://comet.internal.ashler.com; scaffold=https://scaffold.internal.ashler.com; scope=ashler-production; data="$HOME/.comet-native"; port=27653 ;;
  *) echo 'CREW_CHANNEL must be staging or production.' >&2; exit 1 ;;
esac
for tool in node npm python3; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }; done

# Crew setup work also needs protection before the turn-aware engine starts.
marker="$(mktemp /.namespace/tasks/crew-setup.XXXXXXXX)"
trap 'rm -f -- "$marker"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
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
  printf 'export COMET_DEVICE_NAME=%q\n' "$CREW_OWNER_NAME · Devbox · $NAMESPACE_DEVBOX_NAME"
  printf 'export COMET_DATA_DIR=%q\n' "$data"
  printf 'export COMET_EDGE_URL=%q\n' "$edge"
  printf 'export COMET_SCAFFOLD_URL=%q\n' "$scaffold"
  printf 'export COMET_PROJECT_SCOPE=%q\n' "$scope"
  printf 'export COMET_IPC_PORT=%q\n' "$port"
  printf 'export ASHLER_INCREMENTAL_TSC_CHECKS=false\n'
  printf 'export CHROME_PATH=%q\n' "$browser"
  printf 'export PUPPETEER_EXECUTABLE_PATH=%q\n' "$browser"
  printf 'export PLAYWRIGHT_BROWSERS_PATH=%q\n' "$PLAYWRIGHT_BROWSERS_PATH"
  printf 'export NAMESPACE_DEVBOX_NAME=%q\n' "$NAMESPACE_DEVBOX_NAME"
) > "$config"
chmod 600 "$config"
printf '#!/usr/bin/env bash\nset -euo pipefail\nsource %q\nexec %q "$@"\n' "$config" "$CREW_BINARY" > "$HOME/.local/bin/crew-devbox-$CREW_CHANNEL"
chmod 755 "$HOME/.local/bin/crew-devbox-$CREW_CHANNEL"

# Preserve existing personal instructions; replace only this setup's named block.
python3 - "$NAMESPACE_DEVBOX_NAME" <<'PY'
from pathlib import Path
import sys
name = sys.argv[1]
start, end = '<!-- crew-namespace-devbox:start -->', '<!-- crew-namespace-devbox:end -->'
block = f'''{start}
## Namespace Devbox execution
You are running on Namespace Devbox `{name}`, not the user's Mac.
- For browser authentication, print the provider's actual clickable authorization URL and tell the user to open it on their local machine. Do not silently open a remote browser for human login.
- For localhost OAuth callbacks, establish `devbox port-forward {name} --ports PORT:PORT` on the user's machine before opening the URL. Use the port actually reported by the provider; keep the forward alive until login completes. A remote localhost URL alone is not reachable from the user's browser.
- Prefer provider-supported remote/device-code flows when callback forwarding is unavailable (for example `gcloud auth login --no-launch-browser` or the current gcloud remote-bootstrap flow). Follow CLI output; never guess a callback port or manufacture an auth URL. User approval and MFA must remain interactive.
- Never print access/refresh tokens, service-account keys, browser cookies, callback codes, or secret environment files. Do not copy broad local credentials to this machine. Use normal Crew sign-in and Infisical localdev secret rendering, not production credentials.
- Chromium is installed at `~/.local/bin/crew-chromium`. Use it headlessly for automation. Keep CDP and application listeners loopback-only and forward only required ports; never publish an unauthenticated browser debugging port.
- Crew builds with Namespace turn protection manage owned `/.namespace/tasks` markers automatically. On older engines, or for independent commands that outlive a turn, create a uniquely named marker for long-running work and remove only that marker with an EXIT/INT/TERM trap. Do not assume an idle connection protects a task. Never delete other sessions' markers. Markers survive a crash; inspect ownership before removing stale ones.
- Keep `ASHLER_INCREMENTAL_TSC_CHECKS=false`; do not run local typechecks. Linux uses Docker + kind + Tilt, not OrbStack. Do not reset/seed a shared or production database.
{end}'''
for relative in ('.omp/agent/AGENTS.md', '.claude/CLAUDE.md', '.codex/AGENTS.md'):
    path = Path.home() / relative
    old = path.read_text() if path.exists() else ''
    if start in old or end in old:
        if old.count(start) != 1 or old.count(end) != 1 or old.index(start) > old.index(end):
            raise SystemExit(f'Malformed managed guidance block in {path}; refusing to replace it')
        before, rest = old.split(start, 1)
        _, after = rest.split(end, 1)
        new = before + block + after
    else:
        new = old.rstrip() + '\n\n' + block + '\n'
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(new)
PY
"$HOME/.local/bin/crew-chromium" --version
printf 'Configured %s for %s. Existing engines were NOT restarted.\n' "$CREW_CHANNEL" "$CREW_OWNER_NAME"
printf 'Sign in: %s login\nStart: %s headless\n' "$HOME/.local/bin/crew-devbox-$CREW_CHANNEL" "$HOME/.local/bin/crew-devbox-$CREW_CHANNEL"
