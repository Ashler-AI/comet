#!/usr/bin/env bash
# Run inside the owner's Namespace Devbox after installing a verified Crew release.
set -euo pipefail
: "${CREW_OWNER_NAME:?Set the real owner display name}"
: "${NAMESPACE_DEVBOX_NAME:?Set the Namespace Devbox name}"
: "${NAMESPACE_DEVBOX_ID:?Set the immutable ID from devbox list --output json}"
: "${CREW_BINARY:?Set the absolute path to the verified Crew executable}"
CREW_CHANNEL="${CREW_CHANNEL:-staging}"
[[ "$(uname -s)" == Linux && -d /.namespace/tasks ]] || { echo 'Run inside a Namespace Linux Devbox.' >&2; exit 1; }
[[ "$CREW_BINARY" == /* && -x "$CREW_BINARY" ]] || { echo 'CREW_BINARY must be an absolute executable path.' >&2; exit 1; }
[[ "$NAMESPACE_DEVBOX_NAME" =~ ^[a-zA-Z0-9][a-zA-Z0-9._-]*$ ]] || { echo 'Invalid Devbox name.' >&2; exit 1; }
[[ "$NAMESPACE_DEVBOX_ID" =~ ^[a-z0-9]{13}$ ]] || { echo 'Invalid Namespace Devbox ID.' >&2; exit 1; }
[[ "$CREW_OWNER_NAME" != *$'\n'* && -n "${CREW_OWNER_NAME// /}" ]] || { echo 'Invalid owner name.' >&2; exit 1; }
case "$CREW_CHANNEL" in
  staging) edge=https://comet-staging.internal.ashler.com; scaffold=https://scaffold-staging.internal.ashler.com; scope=ashler-staging; data="$HOME/.comet-native-staging"; port=27655; callback_port=27656 ;;
  production) edge=https://comet.internal.ashler.com; scaffold=https://scaffold.internal.ashler.com; scope=ashler-production; data="$HOME/.comet-native"; port=27653; callback_port=27654 ;;
  *) echo 'CREW_CHANNEL must be staging or production.' >&2; exit 1 ;;
esac
for tool in node npm python3 tmux; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }; done

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
  printf 'export COMET_CALLBACK_PORT=%q\n' "$callback_port"
  printf 'export ASHLER_INCREMENTAL_TSC_CHECKS=false\n'
  printf 'export CHROME_PATH=%q\n' "$browser"
  printf 'export PUPPETEER_EXECUTABLE_PATH=%q\n' "$browser"
  printf 'export PLAYWRIGHT_BROWSERS_PATH=%q\n' "$PLAYWRIGHT_BROWSERS_PATH"
  printf 'export NAMESPACE_DEVBOX_NAME=%q\n' "$NAMESPACE_DEVBOX_NAME"
  printf 'export NAMESPACE_DEVBOX_ID=%q\n' "$NAMESPACE_DEVBOX_ID"
) > "$config"
chmod 600 "$config"
printf '#!/usr/bin/env bash\nset -euo pipefail\nsource %q\nexec %q "$@"\n' "$config" "$CREW_BINARY" > "$HOME/.local/bin/crew-devbox-$CREW_CHANNEL"
chmod 755 "$HOME/.local/bin/crew-devbox-$CREW_CHANNEL"
install -m 755 "$(dirname "${BASH_SOURCE[0]}")/autostart.sh" "$HOME/.local/bin/crew-devbox-autostart"
cat > "$HOME/.local/bin/crew-devbox-gcloud-login" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
command -v docker >/dev/null || { echo 'docker is required for gcloud login.' >&2; exit 1; }
# Force gcloud's loopback flow without launching a browser inside the Devbox.
# The setup controller forwards localhost:8085 and opens the printed URL locally.
exec docker run --rm -i --network host \
  -e DISPLAY=:99 -e BROWSER=/bin/echo \
  -v "$HOME/.config/gcloud:/root/.config/gcloud" \
  google/cloud-sdk:slim gcloud auth login --force "$@"
SH
chmod 755 "$HOME/.local/bin/crew-devbox-gcloud-login"

# Preserve existing personal instructions; replace only this setup's named block.
python3 - "$NAMESPACE_DEVBOX_NAME" "$CREW_CHANNEL" <<'PY'
from pathlib import Path
import sys
name = sys.argv[1]
start, end = '<!-- crew-namespace-devbox:start -->', '<!-- crew-namespace-devbox:end -->'
block = f'''{start}
## Namespace Devbox execution
You are running on Namespace Devbox `{name}`, not the user's Mac.
- Human browser approval remains interactive, but credentials must be created and stored on this Devbox; never copy a local credential directory or use gcloud remote bootstrap.
- One-time setup owns the short-lived controller callback tunnel. For gcloud reauthentication, use `~/.local/bin/crew-devbox-gcloud-login`; if localhost:8085 is not already forwarded, rerun the Namespace Devbox skill's authentication phase rather than asking for a separate local coordinating agent.
- Use provider-supported device flows when available. Follow the CLI's actual URL and preserve user approval/MFA; never guess callback ports or manufacture authorization URLs.
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

# Namespace recreates its declared interactive dev-shell on boot. Preserve the
# owner's shell setup and install only this channel's idempotent startup hook.
channel = sys.argv[2]
path = Path.home() / '.bashrc'
old = path.read_text() if path.exists() else ''
start = f'# >>> Crew Devbox {channel} autostart >>>'
end = f'# <<< Crew Devbox {channel} autostart <<<'
for first, last in [(start, end)] + ([('# >>> Crew Devbox autostart >>>', '# <<< Crew Devbox autostart <<<')] if channel == 'staging' else []):
    if first in old or last in old:
        if old.count(first) != 1 or old.count(last) != 1 or old.index(first) > old.index(last):
            raise SystemExit(f'Malformed Crew startup block in {path}')
        before, rest = old.split(first, 1)
        _, after = rest.split(last, 1)
        old = before + after
block = f'{start}\n"$HOME/.local/bin/crew-devbox-autostart" {channel}\n{end}\n'
path.write_text(old.rstrip() + '\n\n' + block)
PY
"$HOME/.local/bin/crew-chromium" --version
printf 'Configured %s for %s. Existing engines were NOT restarted.\n' "$CREW_CHANNEL" "$CREW_OWNER_NAME"
printf 'Sign in: %s login\nStart: %s %s\n' "$HOME/.local/bin/crew-devbox-$CREW_CHANNEL" "$HOME/.local/bin/crew-devbox-autostart" "$CREW_CHANNEL"
