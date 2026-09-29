---
name: namespace-devbox
description: Set up an owner-named Namespace Devbox with Crew, Chromium, forwarded login, and the Ashler Tilt stack. Not for Scaffold or production deployments.
---

# Personal Namespace Devbox

Use when someone asks for their own remote Crew development machine. Reuse an existing owner-matched machine unless a new one was requested. Default: persistent Linux amd64, the smallest current shape with at least 16 CPUs and 64 GiB memory, private access, a one-hour idle timeout, Crew production. This creates billable infrastructure: the user's setup request must authorize the machine. Do not create extra machines to retry an uncertain response; inspect `devbox list` first.

## Identity and prerequisites

- Resolve the actual signed-in owner's name and login, for example `gh api user --jq '{login,name}'`; use a confirmed name if the profile is empty. Never bake the current maintainer's identity into another person's setup.
- Name the machine `<owner-login>-ashler-dev`. Include `<Owner Name> · Devbox · <machine-name>` in its Crew display name and put owner/purpose in Namespace's `--purpose` field. Existing Crew device names survive restarts: rename an existing device through Crew's device controls rather than assuming an environment change renames its synced row.
- Read the target checkout's own guidance. Avoid checking out or resetting a worktree with someone else's changes.
- Install the official Namespace `devbox` CLI and authenticate interactively: https://namespace.so/docs/reference/devbox-cli. Preserve user approval/MFA. Do not copy provider keys or change account permissions to bypass login.
- Inspect `devbox create --help`, `devbox image list --help`, and https://namespace.so/docs/devbox/creating. Size labels can change; select the smallest shape that provides **at least 16 CPUs and 64 GiB**, rather than assuming `l` maps to those resources. After creation, confirm and report the actual shape with `nproc` and `/proc/meminfo`; a current 64 GiB shape may provide 32 CPUs.

Create using the verified size and existing approved image/Blueprint (the official `builtin:agents` image is a starting point):

```bash
devbox create --name "$DEVBOX_NAME" --purpose "$OWNER_NAME's Ashler development machine" \
  --image builtin:agents --size "$VERIFIED_SIZE" --platform linux/amd64 \
  --persistent --access_mode private --auto_stop_idle_timeout 1h \
  --checkout github.com/Ashler-AI/ashler-platform
devbox configure-ssh "$DEVBOX_NAME"
```

Namespace keeps the persistent checkout at `/workspaces/ashler-platform`. Verify that path after creation; do not relocate it into the Devbox home filesystem. Host configuration defaults Crew's folder browser to `/workspaces`, so the checkout is visible immediately instead of opening at `$HOME`. If an approved Blueprint uses a different persistent checkout parent, pass that absolute directory as `CREW_FOLDER_ROOT` when running `configure-host.sh`.

Only use `--setup_github`/`devbox setup-github` after the user explicitly authorizes transferring their local GitHub authentication. Otherwise sign in on the host with GitHub's interactive browser/device flow. Linux uses Docker, not OrbStack; use the same kind/Tilt topology as the Mac without trying to install OrbStack remotely.

## Install the verified Crew release

Pick one explicit released version from the production channel. Read its private immutable `releases/<version>/scaffold-manifest.json` using the owner's authorized GCS access. Validate `releaseSurface=scaffold`, the version, `scaffoldRuntimeVersion` compatibility, and the Linux architecture/digest. Download the exact archive from that same version directory, verify SHA-256 locally, upload it with `devbox upload`, and verify the same SHA-256 on the host before extraction. Do not install a moving `latest` archive against an earlier manifest or trust an unchecked downloaded executable.

Install under `~/.local/lib/crew/<version>/`; extract with `--strip-components=1` only after checking the archive layout. Keep previous versions for rollback. Do not replace/restart the Mac client hosting your current agent turn. A source build is not a released version, and installing a binary does not update a running engine.

The macOS controller and active remote host both need a release containing Devbox support. A current Mac binary paired with an old running engine may still show Local or fail to control remote turns.

Install the OMP version pinned by that Crew release before configuring the host. Read `source.repository` and `source.commit` from the verified manifest, fetch that exact commit's root `install.sh` through the authenticated GitHub API, upload it, and run `install.sh --install-omp` on the Devbox. The installer selects the Linux architecture, verifies the official upstream SHA-256, and installs `~/.local/bin/omp`. Never use a moving OMP release or copy local provider credentials. Confirm `omp --version`; `configure-host.sh` fails closed when the harness is absent.

## Configure Chromium, identity, and agent guidance

Resolve the selected machine's immutable `id` from `devbox list --output json`; do not use its display name as the wake binding. Upload both `scripts/configure-host.sh` and `scripts/autostart.sh` into the same directory on the Devbox, then run:

```bash
CREW_OWNER_NAME="$OWNER_NAME" \
NAMESPACE_DEVBOX_NAME="$DEVBOX_NAME" \
NAMESPACE_DEVBOX_ID="$DEVBOX_ID" \
CREW_BINARY="$HOME/.local/lib/crew/$VERSION/comet" \
CREW_CHANNEL=production \
CREW_FOLDER_ROOT=/workspaces \
  bash /tmp/configure-host.sh
```

Resolve the host `$HOME`, not the Mac's home, when constructing the remote command. The script:

- Installs pinned Playwright Chromium and Linux dependencies by default, without Ubuntu's snap-based Chromium package. Node/npm/Python and sudo access for browser system dependencies are prerequisites.
- Exposes `~/.local/bin/crew-chromium`; stores the browser outside repository checkouts.
- Creates `~/.local/bin/crew-devbox-staging` (or `crew-devbox-production`) with the explicit matching edge, Scaffold URL, project, data directory, IPC port, and owner-qualified name. It does not restart existing engines.
- Opens Crew's folder browser at the persistent Namespace checkout parent (`/workspaces` by default), making `ashler-platform` directly selectable without moving it into the Devbox home filesystem.
- Stores the immutable Namespace ID in the channel configuration so the host publishes `namespaceDevboxId` in its Crew device row. Both controller and host must run a release with this field for the wake control to appear.
- Installs `~/.local/bin/crew-devbox-autostart` and an idempotent, channel-specific `.bashrc` hook. Namespace's declared interactive `dev-shell` runs the hook at boot; it starts a self-respawning session on the dedicated `tmux -L crew-devbox` server without an idle activity marker. Boot and CLI wake use that same server regardless of inherited `TMUX`. It does not restart an already-running engine.
- Adds a bounded managed guidance block to OMP, Claude Code, and Codex personal instructions without replacing existing guidance. It includes clickable login links, callback forwarding, secret handling, and independent task-marker cleanup.
- Configuration changes apply on the next engine start. On a reused machine, inspect active turns before restarting; do not claim the new device identity, immutable Namespace binding, or callback port is live until the restarted engine republishes them.

If the Devbox cannot resolve the browser CDN, do not change its network/security policy. Download the exact archive URL emitted by the pinned Playwright installer on an authorized local machine, compare SHA-256 before and after `devbox upload`, and extract into the exact versioned cache directory reported by `chromium.executablePath()`. Then rerun configuration; it reuses an existing executable. Install required Linux dependencies before the smoke check. Never substitute an unchecked mirror or claim the browser is installed after a failed download.

Use separate staging and production data directories. Do not copy `session.json`, device identity, or provider OAuth tokens across environments.

## Authenticate once, then run transparently

The setup skill owns the unified controller-side forward during initial setup. On the first turn to an online Namespace Devbox, Crew's local controller starts separate bounded forwards for gcloud and available Ashler services without delaying the turn; offline devices are woken first. Never ask the owner to start a second local agent session, copy a credential directory, or manually run `devbox port-forward` for normal Crew use.

Use the production callback port `27654` installed by `configure-host.sh` (`27656` is reserved for staging). Before presenting any authorization URL, check that the required local ports are free, then start one supervised Namespace forward for the whole setup:

```bash
# Avoid the owner's usual local app and Tilt ports by default.
WEB_LOCAL_PORT="${WEB_LOCAL_PORT:-13000}"
TILT_LOCAL_PORT="${TILT_LOCAL_PORT:-20350}"
# Override either local port when it is already occupied.
# Production; staging uses 27656 instead of 27654.
devbox port-forward "$DEVBOX_ID" \
  --ports "27654:27654,8085:8085,$WEB_LOCAL_PORT:3000,$TILT_LOCAL_PORT:10350"
```

Port `8085` is gcloud's loopback callback. Ports `3000` and `10350` are remote app and Tilt ports; their local sides are `$WEB_LOCAL_PORT` and `$TILT_LOCAL_PORT`. Check the chosen local ports immediately before starting the forward. If either application port is occupied, set that variable to an unused local port and use it in every link and health check. Never stop the existing listener. Credential callback ports cannot be remapped because providers bind them into signed OAuth requests; fail closed rather than kill another listener. Keep this single forward alive through authentication and runtime verification. The tunnel transports loopback HTTP only; provider credentials are created and stored on the Devbox.
This unified forward is setup-scoped. Stop it after authentication and endpoint verification—and before testing an ordinary Crew turn—so it does not collide with Crew's automatic `8085` forward or defeat Namespace auto-stop. Crew's relay, not this tunnel, carries coding-session control, transcripts, and streaming output.

Run `~/.local/bin/crew-devbox-production login` on the host and open its printed authorization URL for the owner. The forwarded fixed callback completes sign-in automatically; never ask the owner to paste its callback URL or code unless the provider rejects loopback callbacks.

For gcloud, run `~/.local/bin/crew-devbox-gcloud-login` on the host. It forces gcloud's normal `localhost:8085` browser flow without launching a remote browser; open the printed URL locally and let the existing forward return the callback directly to gcloud on the Devbox. Do not use `--no-browser`, `--no-launch-browser`, remote bootstrap, local credential export, or credential-directory copying for normal setup.

The gcloud helper owns one interactive login at a time. Do not wrap it in a short command timeout: browser approval and MFA can legitimately take several minutes. If the controller disconnects or a login appears stale, inspect the existing Devbox listener/container before retrying. A still-running flow on `8085` remains authoritative; starting another flow makes gcloud choose `8086`, which the fixed callback tunnel intentionally does not forward. Stop only a duplicate flow created by this setup.

GitHub uses its supported device flow and Infisical uses its supported interactive remote login; neither requires local credential copying. Preserve user approval and MFA. Do not distribute production credentials. Render only Infisical `platform/localdev` products for local development.

OMP runs launched by Crew use Crew Agent Auth through a per-run inference route. Do not run OMP `/login`, copy provider credentials, or configure a separate OMP auth broker. A bare interactive `omp` process is not an Agent Auth test; verify models by starting a Crew-owned remote turn with an explicit selected model.

Use the canonical model selector returned by Crew's model catalog, not the display label. For example, `openai-codex/gpt-5.6-luna` is a selector while `gpt-5.6-luna` is only its label; a label-only request can create a user message without a runnable agent turn or task marker.

Start the configured engine with its installed supervisor:

```bash
~/.local/bin/crew-devbox-autostart production
~/.local/bin/crew-devbox-production status
```

The helper uses a channel-specific tmux session and restarts the engine after exit with a five-second delay. It requires `tmux` and Python. Namespace's built-in image does not provide systemd; do not assume `comet daemon install` works there. Keep a declared interactive `dev-shell` session so `.bashrc` runs after a machine restart. A separate systemd-capable image may use Crew's native daemon installer instead, preserving the channel configuration and `NAMESPACE_DEVBOX_ID`; never run competing supervisors on one data directory.

On a fresh machine, start the helper normally. On a reused machine running staging, first verify there are no active turns, then stop only the staging engine before starting production once; never run both channels concurrently. Confirm the production device row is labeled **Devbox** and carries the expected immutable Namespace ID before testing wake.

After one-time setup, ordinary coding sessions need no manual port forward or local coordinating LLM. Crew starts one 30-minute forward for gcloud `8085` and a separate 30-minute service forward for each available local mapping: `http://devbox.localhost:13000` reaches the Ashler app on Devbox port `3000`, and `http://tilt.devbox.localhost:20350` reaches Tilt on port `10350`. An occupied service port is skipped without affecting gcloud or the other service; failures are logged locally and never wake or relink an already-online device. Sending a draft to an offline Namespace device starts the explicit wake flow: the controller uses its installed `devbox` CLI and Namespace login, boots the selected machine, starts the matching channel helper, verifies the exact Crew device through the relay, then reloads refs and models.

The manual wake equivalent is diagnostic only:

```bash
devbox exec "$DEVBOX_ID" -- sh -c 'exec "$HOME/.local/bin/crew-devbox-autostart" production'
```

Do not teach the owner to use that command for normal operation. Provider reauthentication is exceptional and remains an interactive security boundary; rerun this skill's authentication phase so it owns the short-lived callback forward.

## Automatic sleep protection

Namespace treats files directly under `/.namespace/tasks` as active work, not CPU usage: https://namespace.so/docs/guides/devbox/long-running-tasks.

Crew creates one owned marker for each active agent turn. Completion, cancellation, failed starts, session stop, and orderly engine shutdown release Crew's markers. Idle persistent harness processes must not pin the machine awake. Other engines and independent jobs keep their own markers.

Markers survive a crash. No application can run cleanup after SIGKILL or power loss; after such an event, restart Crew so its stale-owner cleanup can run, or inspect and remove only confirmed abandoned Crew markers. Never clear `/.namespace/tasks/*` indiscriminately. A live engine's markers must not be removed by another engine.

For independent work that outlives a Crew turn, use a separate uniquely named marker with a shell EXIT/INT/TERM trap. A background process alone does not extend Crew's turn marker. Removing the last marker restarts Namespace's normal idle countdown; active connections may still keep the machine awake.

## Ashler local stack and setup-scoped forwarding

In the checked-out Ashler Platform repository read `docs/agents/localdev.md` and `infra/localdev/README.md`. Install/verify Docker, Node/npm, kubectl, kind, ctlptl, Tilt, just, gcloud and Infisical using their maintained Linux installation paths. Follow that repository's setup tooling; do not create a competing stack implementation here.

Keep `ASHLER_INCREMENTAL_TSC_CHECKS=false` and do not run local typechecks. Once Infisical localdev authentication is ready, use the repository's `just bootstrap`, `just env`, and `just dev`. Seed only the confirmed sandbox-local database when needed; never reset a shared database to make setup pass. Run Tilt in another persistent Namespace terminal.

The unified setup forward exposes the app at `http://devbox.localhost:$WEB_LOCAL_PORT` and Tilt at `http://tilt.devbox.localhost:$TILT_LOCAL_PORT`; these `*.localhost` names remain local to the owner's machine. Give the owner clickable links using the actual selected values only after both endpoints respond, then stop the setup forward before testing Crew's automatic forwards. Do not expose Tilt or browser CDP through `devbox url`. A Namespace `devbox.so` URL is optional, supports only `private` or `workspace` access, and must be created only after the owner authorizes that broader access.

Crew wake proves the engine and relay are back; it does not prove the host-local kind/Tilt stack survived the machine stop. After every explicit stop/wake verification, recheck the app and Tilt endpoints. If Kubernetes reports a refused stale API-server port, use the maintained `just env` path to reconcile ctlptl/kind and regenerate kubeconfig, then restart the dedicated Tilt tmux session and wait for `critical-path-ready` on port `10350`. Reconciliation may recreate the ephemeral local cluster and database, so let the normal seed resource run again; never apply this recovery to a shared or snapshot database.

## Completion evidence

Do not stop at installation. Record:

1. Namespace machine name, verified owner, actual CPU/memory capacity, and active production Crew version/environment.
2. Both Mac and Devbox sessions visible in Crew, with this host labeled **Devbox** and the owner-qualified device name. Stop the Devbox and confirm the same offline row remains reachable in **Settings → Devices** and the add-folder picker.
3. A Mac-initiated turn that runs `hostname`/`pwd` on the Devbox and streams its real result back.
4. Marker present while that turn is active and absent after completion/cancellation; shutdown clears only that engine's markers. Do not restart a user's active engine without coordinating their running turns.
5. Headless Chromium successfully renders a page (not merely `--version`) and the forwarded app/Tilt endpoints respond.
6. Login success without tokens in logs. Explain how to reconnect, log in through forwarded callbacks, and clean up truly stale markers.
7. Explicitly stop an idle Devbox, then use Wake and connect or a pending send. Verify the same device ID returns, refs load, the draft submits once, and the app/Tilt endpoints recover; also verify passive sidebar viewing leaves a stopped machine stopped.

State any blocked authentication, missing release, or unverified UI explicitly. Do not claim an installed or locally tested implementation is already released.
