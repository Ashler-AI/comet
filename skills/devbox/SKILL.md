---
name: devbox
description: Set up an owner-named Namespace or Boat devbox with Crew, Chromium, private login forwarding and Ashler Tilt. Not Scaffold or production deployment.
---

# Personal devbox

Use for a personal persistent Linux amd64 Crew machine. Target **16 CPUs / 32 GB**; verify the provider's current shape and actual `nproc` and `/proc/meminfo`. Reuse an owner-matched machine unless a new one was requested. A setup request authorizes one billable machine, not an account plan purchase, broader sharing, or replacement after an uncertain response.

Default to Crew production; use staging when requested or when testing against the owner's staging controller. Never replace or restart the Mac client hosting the active agent turn. Controller and host must both run a verified release with neutral devbox bindings before claiming wake support.

## Provider and identity

Resolve the signed-in owner through `gh api user --jq '{login,name}'`; use a confirmed name if the profile is empty. Name the machine `<owner-login>-ashler-dev` and the Crew device `<Owner Name> · Devbox · <machine-name>`. Existing device names survive reboot; rename a reused device through Crew rather than assuming an environment change changes its row.

Read only the selected provisioning variant:

- [Namespace](references/namespace.md): `devbox`, persistent `/workspaces`, task-marker idle protection.
- [Boat](references/boat.md): `boat`, `xlarge`, persistent `/home/user`, finite TTL protection.

Preserve interactive provider sign-in, approval and MFA. Never manufacture authorization URLs, answer provider safety gates automatically, transfer broad local credentials, change account permissions, or enable public hosting to bypass setup failures. Read the target checkout's guidance and preserve existing worktrees.

## Verified Crew and OMP

Select one immutable released version from the requested channel. Read that version's private `scaffold-manifest.json` using authorized GCS access. Verify release surface, version, runtime compatibility, architecture and digest. Download the archive from the same version directory and verify SHA-256 before transfer and again on the host. Inspect archive layout before extraction. Install under `~/.local/lib/crew/<version>/` and retain the previous version for rollback. A source build is not a release.

From the verified manifest's `source.repository` and `source.commit`, fetch that exact commit's `install.sh` and the three files under `skills/devbox/scripts/`: `configure-host.sh`, `autostart.sh`, and `keepalive.py`. Use authenticated GitHub access and immutable source, not a moving main/latest script. The scripts in this repository are the single shared implementation for both providers; do not maintain a second copy in Ashler Platform.

Run the pinned installer with `--install-omp` on the host and confirm `omp --version`. Never copy model-provider OAuth tokens or install a moving OMP version. Crew-owned turns use Crew Agent Auth; do not run OMP `/login` or set up a separate broker.

## Configure the host

Install/verify Node/npm, Python, gcloud, passwordless sudo, and the selected provider's prerequisites. Namespace needs tmux; Boat needs its official CLI and a sandbox-scoped credential with `sandbox.update`. Confirm Boat `extend` succeeds before lengthy setup; never substitute an account-wide token. Render only Infisical `platform/localdev` products for the Ashler stack.

Upload all three shared scripts into the same directory. Use the host's home path, then run:

```bash
CREW_OWNER_NAME="$OWNER_NAME" \
CREW_DEVBOX_NAME="$DEVBOX_NAME" \
COMET_DEVBOX_PROVIDER="$PROVIDER" \
COMET_DEVBOX_ID="$DEVBOX_ID" \
CREW_BINARY="$HOME/.local/lib/crew/$VERSION/comet" \
CREW_CHANNEL="$CHANNEL" \
CREW_FOLDER_ROOT="$CHECKOUT_PARENT" \
  bash /tmp/configure-host.sh
```

Boat's configured ID must match the native `BOAT_ID`. A copied template binding fails closed. Configuration protects its own execution: owned Namespace marker or a renewable Boat lease. It installs pinned Playwright Chromium outside `~/.cache`, channel-qualified Crew/gcloud helpers, and a bounded managed guidance block without replacing personal instructions. Existing engines are not restarted.

If Chromium cannot reach its CDN, use the exact pinned installer URL on an authorized local machine, compare SHA-256 before and after transfer, and extract into the reported cache directory. Do not substitute mirrors or change network/security policy. Prove a rendered page, not just `--version`.

Keep staging and production data directories separate. Do not copy `session.json`, device IDs, or credentials across environments. On a reused machine, inspect active turns before any restart; stop only the selected owned engine after work is idle. Never run competing channels or supervisors against one data directory.

## Private setup forwarding and sign-in

Before presenting login URLs, verify the required local ports are free. Credential callbacks cannot be remapped; fail closed rather than stop another listener. App/Tilt local ports can use an unused alternative, consistently in every link and health check.

| Purpose | Local | Remote |
| - | - | - |
| Crew login | production `27654`, staging `27656` | same |
| gcloud login | `8085` | `8085` |
| App | `13000` | `3000` |
| Tilt | `20350` | `10350` |

Use the selected provider's forwarding commands for all four mappings. Keep the setup-scoped forwards alive through authentication and endpoint checks, then stop them **before an ordinary Crew turn**. Never expose Tilt, callbacks, or browser CDP through public hosting. The coding session travels over Crew's relay, not these tunnels.

Run `~/.local/bin/crew-devbox-$CHANNEL login` on the host; open its actual printed authorization URL locally and let the fixed forwarded callback complete login. For gcloud, use `~/.local/bin/crew-devbox-gcloud-login` and the `8085` forward. Do not use remote bootstrap, `--no-browser`, or copied credentials. Inspect a pending login before retrying; duplicate flows can select `8086` and strand the callback. GitHub and Infisical use their supported interactive/device flows.

Start the host with `~/.local/bin/crew-devbox-autostart "$CHANNEL"`. Namespace uses the dedicated self-respawning tmux server; Boat uses Crew's native systemd user daemon with linger. Boat imports the current verified `BOAT_ID` into the service manager, never into a snapshotted unit file. If native identity is unavailable at OS boot, the engine fails closed until an explicit trusted Crew wake supplies it.

## Work and sleep

Crew protects executing turns and releases protection on completion, cancellation, failed starts and shutdown. Parked harnesses and idle Tilt processes do not keep a host running.

- Namespace uses owned `/.namespace/tasks` markers. Crashes can leave stale files; remove only confirmed abandoned markers, never all tasks.
- Boat uses a one-hour finite TTL, renewed every five minutes while work is active. Its default TTL is **not idle detection**. Initial protection failure rejects work; renewal failure interrupts protected work and leaves it recoverable. Last completion resets the idle deadline once. A crash leaves the last finite deadline, not indefinite billing. Provider/control-plane outages can still interrupt work.
- For independent work outside a turn, use `~/.local/bin/crew-devbox-keepalive <command> [arguments...]` with the selected channel configuration loaded. Do not launch background work and assume the finished turn still protects it.

Ordinary desktop turns automatically prepare bounded private gcloud/app/Tilt forwards; occupied service ports are skipped independently. An explicit wake or pending send boots only the selected bound machine, confirms the exact Crew peer, reloads selectors and submits an unchanged draft once. Passive viewing does not wake a stopped device. Provider login failure never creates a replacement.

## Ashler runtime and proof

Read the checkout's `docs/agents/localdev.md` and `infra/localdev/README.md`; use maintained Linux installation paths for Docker, kubectl, kind, ctlptl, Tilt, just, gcloud and Infisical. Follow `just bootstrap`, `just env`, and `just dev`, not a competing stack. Retain the maintained Next.js binding (`0.0.0.0` by default): Docker/kind must reach `http://host.docker.internal:3000` for callbacks. Deny public inbound app traffic through provider private access or the host firewall while allowing required Docker bridge traffic. Keep Tilt/CDP and credential callbacks loopback-only; never publish them through provider hosting. Keep `ASHLER_INCREMENTAL_TSC_CHECKS=false`; never run local typechecks.

Seed only the confirmed host-local database. Boat setup exports `ASHLER_LOCALDEV_DATA_DIR=$HOME/.local/share/ashler-localdev` into the channel config, noninteractive shell and systemd engine; Namespace and ordinary local defaults remain unchanged. Back up and verify existing stores before an explicit migration; never reseed on wake. After stop/resume, verify app and Tilt separately from Crew readiness. Never apply local recovery to shared or managed snapshot databases.

Record actual owner, provider ID, CPU/memory/disk, Crew version/channel, and these observations:

1. Both Mac and host visible, owner-qualified Devbox label, same stopped device retained in Devices/folder picker.
2. Mac-initiated real selected-model turn returns `hostname`/`pwd` through Crew Agent Auth.
3. Active turn remains protected, including a turn exceeding one hour; overlapping completion/cancellation preserves other work; idle sleep follows final completion.
4. Headless Chromium renders a page; private app/Tilt endpoints and pod-to-web callback reachability work before and after resume. Forwarded health alone does not prove document generation/search/research callbacks.
5. Explicit stop/wake returns the same device ID, reloads refs/models, submits the draft once, and passive viewing leaves a stopped machine stopped.
6. Measure 32 GB peak memory during cold bootstrap, rebuild, browser and concurrent turns. Container limits are not measured use; leave optional data/observability stacks opt-in.

State authentication, release, UI or runtime proof gaps explicitly. Installation, tests and a ready relay alone do not prove the complete setup.
