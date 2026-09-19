---
name: namespace-devbox
description: Set up an owner-named Namespace Devbox with Crew, Chromium, forwarded login, and the Ashler Tilt stack. Not for Scaffold or production deployments.
---

# Personal Namespace Devbox

Use when someone asks for their own remote Crew development machine. Reuse an existing owner-matched machine unless a new one was requested. Default: persistent Linux amd64, 16 CPUs, 64 GiB memory, private access, a one-hour idle timeout, Crew staging. This creates billable infrastructure: the user's setup request must authorize the machine. Do not create extra machines to retry an uncertain response; inspect `devbox list` first.

## Identity and prerequisites

- Resolve the actual signed-in owner's name and login, for example `gh api user --jq '{login,name}'`; use a confirmed name if the profile is empty. Never bake the current maintainer's identity into another person's setup.
- Name the machine `<owner-login>-ashler-dev`. Include `<Owner Name> · Devbox · <machine-name>` in its Crew display name and put owner/purpose in Namespace's `--purpose` field. Existing Crew device names survive restarts: rename an existing device through Crew's device controls rather than assuming an environment change renames its synced row.
- Read the target checkout's own guidance. Avoid checking out or resetting a worktree with someone else's changes.
- Install the official Namespace `devbox` CLI and authenticate interactively: https://namespace.so/docs/reference/devbox-cli. Preserve user approval/MFA. Do not copy provider keys or change account permissions to bypass login.
- Inspect `devbox create --help`, `devbox image list --help`, and https://namespace.so/docs/devbox/creating. Size labels can change; verify the selected size is **16 CPU / 64 GiB** rather than assuming `l` always means that. After creation, confirm with `nproc` and `/proc/meminfo`.

Create using the verified size and existing approved image/Blueprint (the official `builtin:agents` image is a starting point):

```bash
devbox create --name "$DEVBOX_NAME" --purpose "$OWNER_NAME's Ashler development machine" \
  --image builtin:agents --size "$VERIFIED_SIZE" --platform linux/amd64 \
  --persistent --access_mode private --auto_stop_idle_timeout 1h \
  --checkout github.com/Ashler-AI/ashler-platform
devbox configure-ssh "$DEVBOX_NAME"
```

Only use `--setup_github`/`devbox setup-github` after the user explicitly authorizes transferring their local GitHub authentication. Otherwise sign in on the host with GitHub's interactive browser/device flow. Linux uses Docker, not OrbStack; use the same kind/Tilt topology as the Mac without trying to install OrbStack remotely.

## Install the verified Crew release

Pick one explicit released version from the requested channel. Read its private immutable `releases/<version>/scaffold-manifest.json` using the owner's authorized GCS access. Validate `releaseSurface=scaffold`, the version, `scaffoldRuntimeVersion` compatibility, and the Linux architecture/digest. Download the exact archive from that same version directory, verify SHA-256 locally, upload it with `devbox upload`, and verify the same SHA-256 on the host before extraction. Do not install a moving `latest` archive against an earlier manifest or trust an unchecked downloaded executable.

Install under `~/.local/lib/crew/<version>/`; extract with `--strip-components=1` only after checking the archive layout. Keep previous versions for rollback. Do not replace/restart the Mac client hosting your current agent turn. A source build is not a released version, and installing a binary does not update a running engine.

The macOS controller and remote host both need a release containing Devbox support. A current Mac binary paired with an old running engine may still show Local or fail to control remote turns.

## Configure Chromium, identity, and agent guidance

Upload this skill's `scripts/configure-host.sh`, then run it **inside the Devbox** with:

```bash
CREW_OWNER_NAME="$OWNER_NAME" \
NAMESPACE_DEVBOX_NAME="$DEVBOX_NAME" \
CREW_BINARY="$HOME/.local/lib/crew/$VERSION/comet" \
CREW_CHANNEL=staging \
  bash /tmp/configure-host.sh
```

Resolve the host `$HOME`, not the Mac's home, when constructing the remote command. The script:

- Installs pinned Playwright Chromium and Linux dependencies by default, without Ubuntu's snap-based Chromium package. Node/npm/Python and sudo access for browser system dependencies are prerequisites.
- Exposes `~/.local/bin/crew-chromium`; stores the browser outside repository checkouts.
- Creates `~/.local/bin/crew-devbox-staging` (or `crew-devbox-production`) with the explicit matching edge, Scaffold URL, project, data directory, IPC port, and owner-qualified name. It does not restart existing engines.
- Adds a bounded managed guidance block to OMP, Claude Code, and Codex personal instructions without replacing existing guidance. It includes clickable login links, callback forwarding, secret handling, and independent task-marker cleanup.

If the Devbox cannot resolve the browser CDN, do not change its network/security policy. Download the exact archive URL emitted by the pinned Playwright installer on an authorized local machine, compare SHA-256 before and after `devbox upload`, and extract into the exact versioned cache directory reported by `chromium.executablePath()`. Then rerun configuration; it reuses an existing executable. Install required Linux dependencies before the smoke check. Never substitute an unchecked mirror or claim the browser is installed after a failed download.

Use separate staging and production data directories. Do not copy `session.json`, device identity, or provider OAuth tokens across environments.

## Authenticate and run persistently

Run `~/.local/bin/crew-devbox-staging login` on the host. Present its actual authorization URL as a clickable link to the owner. Before they open it, establish a local tunnel for the **actual callback port reported by Crew**:

```bash
devbox port-forward "$DEVBOX_NAME" --ports "$CALLBACK_PORT:$CALLBACK_PORT"
```

Keep that forwarding process running until the CLI confirms successful login. If the port is occupied locally, do not kill another listener: use the provider's supported remote/device-code flow or the CLI's supported final-callback paste mechanism, treating the callback URL as a credential (never log it).

For gcloud/Infisical/GitHub use their supported interactive remote login. Print the actual provider authorization link and use the callback port they report; do not assume every provider uses Crew's port. `gcloud auth login --no-launch-browser` and the current `--no-browser` remote-bootstrap flow are alternatives; follow the installed CLI's output. Do not sync entire credential directories or distribute production credentials. Render only Infisical `platform/localdev` products for local development.

Start headless Crew in a persistent Namespace terminal session:

```bash
devbox session connect "$DEVBOX_NAME" --session crew-staging
# Inside that terminal:
~/.local/bin/crew-devbox-staging headless
```

A detached terminal keeps the process alive across disconnects but does **not** replace activity markers. Use `crew-devbox-staging status` to verify the signed-in account, edge/project, engine PID, and IPC listener. If using a service manager instead, preserve the generated channel environment and ensure its stop signal reaches Crew; do not run two engines on the same data directory.

## Automatic sleep protection

Namespace treats files directly under `/.namespace/tasks` as active work, not CPU usage: https://namespace.so/docs/guides/devbox/long-running-tasks.

Crew creates one owned marker for each active agent turn. Completion, cancellation, failed starts, session stop, and orderly engine shutdown release Crew's markers. Idle persistent harness processes must not pin the machine awake. Other engines and independent jobs keep their own markers.

Markers survive a crash. No application can run cleanup after SIGKILL or power loss; after such an event, restart Crew so its stale-owner cleanup can run, or inspect and remove only confirmed abandoned Crew markers. Never clear `/.namespace/tasks/*` indiscriminately. A live engine's markers must not be removed by another engine.

For independent work that outlives a Crew turn, use a separate uniquely named marker with a shell EXIT/INT/TERM trap. A background process alone does not extend Crew's turn marker. Removing the last marker restarts Namespace's normal idle countdown; active connections may still keep the machine awake.

## Ashler local stack and forwarding

In the checked-out Ashler Platform repository read `docs/agents/localdev.md` and `infra/localdev/README.md`. Install/verify Docker, Node/npm, kubectl, kind, ctlptl, Tilt, just, gcloud and Infisical using their maintained Linux installation paths. Follow that repository's setup tooling; do not create a competing stack implementation here.

Keep `ASHLER_INCREMENTAL_TSC_CHECKS=false` and do not run local typechecks. Once Infisical localdev authentication is ready, use the repository's `just bootstrap`, `just env`, and `just dev`. Seed only the confirmed sandbox-local database when needed; never reset a shared database to make setup pass. Run Tilt in another persistent Namespace terminal.

Forward only required services, checking local port conflicts first:

```bash
devbox port-forward "$DEVBOX_NAME" --ports 3000:3000,10350:10350
```

The CLI accepts a comma-separated `--ports` list. Give the owner clickable links: http://localhost:3000 and http://localhost:10350. Use alternate local ports if occupied, and update the links. Keep credential callbacks and browser CDP loopback-only; never publish unauthenticated debugging endpoints via `devbox url`.

## Completion evidence

Do not stop at installation. Record:

1. Namespace machine name, verified owner, 16 CPU / 64 GiB capacity, and running Crew version/environment.
2. Both Mac and Devbox sessions visible in Crew, with this host labeled **Devbox** and the owner-qualified device name.
3. A Mac-initiated turn that runs `hostname`/`pwd` on the Devbox and streams its real result back.
4. Marker present while that turn is active and absent after completion/cancellation; shutdown clears only that engine's markers. Do not restart a user's active engine without coordinating their running turns.
5. Headless Chromium successfully renders a page (not merely `--version`) and the forwarded app/Tilt endpoints respond.
6. Login success without tokens in logs. Explain how to reconnect, log in through forwarded callbacks, and clean up truly stale markers.

State any blocked authentication, missing release, or unverified UI explicitly. Do not claim an installed or locally tested implementation is already released.
