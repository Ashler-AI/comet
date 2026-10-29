---
name: devbox
description: Set up an owner-named Boat devbox, or Namespace on request, with Crew, Chromium and private Ashler Tilt. Not Scaffold or production deployment.
---

# Personal devbox

Use for a personal persistent Linux amd64 Crew machine. **Default new machines to Boat `xlarge` (16 CPUs / 32 GB)**; keep an explicitly selected or recovered Namespace machine on its supported `l` shape. Verify the provider's current shape and actual `nproc` and `/proc/meminfo`. Reuse an owner-matched machine for the selected provider unless a new one was requested. A setup request authorizes one billable machine, not an account plan purchase, broader sharing, or replacement after an uncertain response.

Default to Crew production; use staging when requested or when testing against the owner's staging controller. Never replace or restart the Mac client hosting the active agent turn. Controller and host must both run a verified release with neutral devbox bindings before claiming wake support.
Native WakeDevice in this port is restricted to an authenticated local synced **staging** controller in project/deployment `ashler-staging`; this is not a production wake rollout. Do not claim native Namespace wake or active-turn behavior is verified in either channel without the actual end-to-end evidence. Preserve existing Namespace production release/tmux setup independently.

## Provider and identity

Resolve the signed-in owner through `gh api user --jq '{login,name}'`; use a confirmed name if the profile is empty. Name the machine `<owner-login>-ashler-dev` and the Crew device `<Owner Name> · Devbox · <machine-name>`. Existing device names survive reboot; rename a reused device through Crew rather than assuming an environment change changes its row.

Read only the selected provisioning variant:

- [Namespace](references/namespace.md): `devbox`, persistent `/workspaces`, task-marker idle protection.
- [Boat](references/boat.md): `boat`, `xlarge`, persistent `/home/user`, finite TTL protection.

Preserve interactive provider sign-in, approval and MFA. Never manufacture authorization URLs, answer provider safety gates automatically, transfer broad local credentials, change account permissions, or enable public hosting to bypass setup failures. Read the target checkout's guidance and preserve existing worktrees.

## Verified Crew and OMP

Select one immutable released version from the requested channel's authorized private feed. Default channel remains production; a staging request selects only the staging feed, never production aliases. Resolve the channel's `scaffold-manifest.json`, then read `releases/<version>/scaffold-manifest.json` and verify the immutable manifest matches the selection. Require schema 2, release surface `scaffold`, expected version and runtime contract, source repository/40-hex commit/workflow-run provenance, and the exact Linux architecture's archive SHA-256 in `files`. Confirm successful release verification for that source run. Never treat an unpublished checkout or a source build as an installed release.
For this native bootstrap require runtime contract `scaffold.comet-runtime.v2` and the actual native CLI/source guard contract. A staging feed may still point to an older v1 release while a native release exists only at an immutable candidate path. An explicitly requested, successfully verified immutable staging candidate can be selected directly; do not assume `latest` is native, advance aliases, or relax compatibility to get an installation through. An older valid Namespace release must instead use its own selected source workflow.

Use the verified manifest's `source.repository` and immutable `source.commit` to fetch the workflow **before choosing its scripts**. For example, after saving the verified immutable manifest as `manifest.json`:

```bash
REPOSITORY="$(jq -er '.source.repository' manifest.json)"
SOURCE_COMMIT="$(jq -er '.source.commit' manifest.json)"
[[ "$REPOSITORY" == Ashler-AI/comet && "$SOURCE_COMMIT" =~ ^[0-9a-f]{40}$ ]] || exit 1
gh api -H 'Accept: application/vnd.github.raw+json' \
  "repos/$REPOSITORY/contents/skills/devbox/SKILL.md?ref=$SOURCE_COMMIT" > selected-devbox-skill.md
```

Read that selected workflow and its provider reference. Fetch `install.sh`, the selected reference and its complete shared script set through the same authenticated `gh api ...?ref=$SOURCE_COMMIT` route. This native workflow requires `skills/devbox/scripts/{configure-host.sh,configure-host.py,autostart.sh,keepalive.py}` together. Do not fetch any file from moving main/latest, substitute a local guard, or maintain a second implementation in Platform. If the chosen release predates a required script, stop or select an authorized compatible release; do not splice source generations.

Download the archive named in `manifest.json.files` from that **same immutable version directory** using authorized GCS access. Verify its SHA-256 locally, after transfer, and before extraction. Inspect archive paths/types first; reject traversal, unexpected links and layout. Extract into a new owner-private `~/.local/lib/crew/<version>-<archive-sha256>/` directory; retain the previous release. Validate the extracted `comet` against that release's `COMET_SHA256SUMS` and preserve its raw SHA-256 as `CREW_BINARY_SHA256`. Do not run the root installer's normal moving-latest/service path: it can select a different archive and install a production service.

**Existing released setups remain supported:** follow the workflow and actual CLI shipped at the verified manifest's source commit, retaining the selected provider. A maintained older Namespace or Boat production release may use the original three-script workflow and its original native profile/ports. Do not force this native bootstrap or an offline profile migration onto it, and do not claim new native wake/forwarding features are verified merely because its existing released setup works. New native staging setup uses the coordinates below; this assignment publishes no production release.

Run the pinned installer with `--install-omp` on the host and confirm `omp --version`. Never copy model-provider OAuth tokens or install a moving OMP version. Crew-owned turns use Crew Agent Auth; do not run OMP `/login` or set up a separate broker.

## Configure the host

Install/verify Node/npm, Python, gcloud, passwordless sudo, and the selected provider's prerequisites. Namespace needs tmux; Boat needs its official CLI and a sandbox-scoped credential with `sandbox.update`. Confirm Boat `extend` succeeds before lengthy setup; never substitute an account-wide token. Render only Infisical `platform/localdev` products for the Ashler stack.

Upload the selected workflow's complete script set into one private directory. For this native workflow, use the host's actual immutable extracted executable path and raw executable checksum, then run:

```bash
CREW_OWNER_NAME="$OWNER_NAME" \
CREW_DEVBOX_NAME="$DEVBOX_NAME" \
COMET_DEVBOX_PROVIDER="$PROVIDER" \
COMET_DEVBOX_ID="$DEVBOX_ID" \
CREW_BINARY="$IMMUTABLE_CREW_EXECUTABLE" \
CREW_BINARY_SHA256="$VERIFIED_EXECUTABLE_SHA256" \
CREW_CHANNEL="$CHANNEL" \
  bash /tmp/configure-host.sh
```

Boat's configured ID must match the native `BOAT_ID`. A copied template binding fails closed. Configuration protects its own execution: owned Namespace marker or a renewable Boat lease. It installs pinned Playwright Chromium outside `~/.cache`, channel-qualified Crew/gcloud helpers, and a bounded managed guidance block without replacing personal instructions. Existing engines are not restarted.

If Chromium cannot reach its CDN, use the exact pinned installer URL on an authorized local machine, compare SHA-256 before and after transfer, and extract into the reported cache directory. Do not substitute mirrors or change network/security policy. Prove a rendered page, not just `--version`.

Keep staging and production data directories separate. Do not copy `session.json`, device IDs, or credentials across environments. On a reused machine, inspect active turns before any restart; stop only the selected owned engine after work is idle. Never run competing channels or supervisors against one data directory.
This native runtime uses production `~/.crew` / IPC `27958`, or staging `~/.crew-staging` / IPC `27957`; callbacks remain `27654` / `27656`. The shared Linux archive has a production build identity; staging supplies explicit CLI/runtime coordinates, not a private Canary build. Owner labeling uses the actual native `ZERON_DEVICE_NAME`; callback configuration uses `ZERON_CALLBACK_PORT`. Staging Edge/control plane/project/deployment are `comet-staging.internal.ashler.com`, `scaffold-staging.internal.ashler.com`, `ashler-staging`, `ashler-staging`. Production retains its ordinary project and omits deployment unless supplied by its validated release contract.

The guard requires a physical, owner-owned, non-shared-writable executable outside either profile's moving `app` tree and refuses different existing configuration or helper files. Existing native profiles require explicit `CREW_EXISTING_DEVICE_ID` adoption with matching saved coordinates; no credentials or identities are copied. Legacy profiles are retained, never adopted by renaming their directory. Add the actual checkout path through Crew's project picker; there is no invented native default-folder flag.


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

Start the host with `~/.local/bin/crew-devbox-autostart "$CHANNEL"`. Namespace retains its dedicated self-respawning tmux server and declared `.bashrc` boot hook. Boat uses one guarded, channel-qualified `crew-devbox-$CHANNEL.service` running the native `headless` command with explicit coordinates and linger; do not run native `daemon install` over another channel's service. The immutable binary digest is rechecked before every service start. An occupied IPC port is accepted only for the expected owned process: its kernel executable image must match pinned SHA-256, size, owner and mode, and that PID must own the LISTEN socket inode. Cold-restore relocation is permitted; a matching pathname is never authority. Boat imports current verified `BOAT_ID` into the service manager, never a snapshotted unit file. If native identity is unavailable at OS boot, startup fails closed until an explicit trusted Crew wake supplies it. Already-active owned engines are not restarted. Only an already-enabled existing `ashler-localdev.service` is resumed; this does not prove app/Tilt readiness.

## Work and sleep

Crew protects executing turns and releases protection on completion, cancellation, failed starts and shutdown. Parked harnesses and idle Tilt processes do not keep a host running.

- Namespace uses owned `/.namespace/tasks` markers. Crashes can leave stale files; remove only confirmed abandoned markers, never all tasks.
- Boat uses a one-hour finite TTL, renewed every five minutes while work is active. Its default TTL is **not idle detection**. Initial protection failure rejects work; renewal failure interrupts protected work and leaves it recoverable. Last completion resets the idle deadline once. A crash leaves the last finite deadline, not indefinite billing. Provider/control-plane outages can still interrupt work.
- For independent work outside a turn, load the selected config with `set -a; source "$HOME/.config/crew-devbox/$CHANNEL.env"; set +a`, then use `~/.local/bin/crew-devbox-keepalive <command> [arguments...]`. `--timeout SECONDS -- command` optionally bounds work; it never replaces finite Boat TTL renewal. Do not launch background work and assume the finished turn still protects it. Cleanup retains the process-group leader until descendants receive the final signal; cancellation cannot target a reused group ID.

The matching native controller's explicit Wake or pending send must boot only the selected bound machine, confirm the exact Crew peer, reload selectors and submit the unchanged draft once. Passive viewing must not wake a stopped device; provider login failure must not create a replacement. Automatic private app/Tilt/gcloud forwarding is release-dependent: use the selected source's documented support, retain manual setup forwards when needed, and do not describe unimplemented native forwarding as verified.

## Ashler runtime and proof

Read the checkout's `docs/agents/localdev.md` and `infra/localdev/README.md`; use maintained Linux installation paths for Docker, kubectl, kind, ctlptl, Tilt, just, gcloud and Infisical. Follow `just bootstrap`, `just env`, and `just dev`, not a competing stack. Retain the maintained Next.js binding (`0.0.0.0` by default): Docker/kind must reach `http://host.docker.internal:3000` for callbacks. Deny public inbound app traffic through provider private access or the host firewall while allowing required Docker bridge traffic. Keep Tilt/CDP and credential callbacks loopback-only; never publish them through provider hosting. Keep `ASHLER_INCREMENTAL_TSC_CHECKS=false`; never run local typechecks.

Seed only the confirmed host-local database. After stop/resume, verify app and Tilt separately from Crew readiness. Use maintained local recovery instructions for stale kind networking; never reset, migrate or reseed data merely to make wake pass, and never apply local recovery to shared databases. This skill does not select or promise datastore persistence.

Record actual owner, provider ID, CPU/memory/disk, Crew version/channel, and these observations:

1. Both Mac and host visible, owner-qualified Devbox label, same stopped device retained in Devices/folder picker.
2. Mac-initiated real selected-model turn returns `hostname`/`pwd` through Crew Agent Auth.
3. Active turn remains protected, including a turn exceeding one hour; overlapping completion/cancellation preserves other work; idle sleep follows final completion.
4. Headless Chromium renders a page; private app/Tilt endpoints and pod-to-web callback reachability work before and after resume. Forwarded health alone does not prove document generation/search/research callbacks.
5. Explicit stop/wake returns the same device ID, reloads refs/models, submits the draft once, and passive viewing leaves a stopped machine stopped.
6. Measure 32 GB peak memory during cold bootstrap, rebuild, browser and concurrent turns. Container limits are not measured use; leave optional data/observability stacks opt-in.

State authentication, release, UI or runtime proof gaps explicitly. Installation, tests and a ready relay alone do not prove the complete setup.

## Maintainer regression commands

After integration, run `python3 -m unittest discover -s skills/devbox/scripts -p 'test_*.py'` on Linux, plus `bash -n skills/devbox/scripts/{configure-host.sh,autostart.sh}`. The native OS regression checks relocated executable images, same-size wrong digests and PID-owned listener sockets. Follow with an explicitly authorized isolated staging host smoke; no test or installation alone satisfies the completion evidence above.
