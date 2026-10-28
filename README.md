# Zeron

Control your coding agents (Claude Code, Codex, Cursor, Devin, Grok, Hermes, Pi, Antigravity) locally by default, with optional multi-device sync.

*English | [简体中文](README.zh-CN.md) | [한국어](README.ko.md) | [日本語](README.ja.md) | [Español](README.es.md)*

![Zeron desktop app](docs/media/readme/app-screenshot.jpg)

## Desktop app

Download the latest release for your platform from [GitHub Releases](https://github.com/zeronsh/zeron/releases/latest):

- **macOS** — `zeron-<version>-macos-arm64.dmg`
- **Windows** — `zeron-<version>-windows-x86_64-setup.exe`
- **Linux** — `zeron-<version>-linux-<arch>.tar.gz`, then run its `install.sh`

No account or network connection is needed; sessions stay on your device. The app updates itself.

## Headless (CLI)

For servers and other machines without a display, such as a VPS that keeps agents running after you close your laptop. Linux only:

```bash
curl -fsSL https://zeron.sh/install.sh | sh
zeron status
```

The installer starts the engine as a background service that survives reboots.

```bash
zeron status      # local/synced mode and engine status
zeron update      # update to the latest release
zeron daemon start|stop|restart|status
```

### Native session search

`comet session search --query "title words" --limit 50` searches the signed-in
controller's own directory entries; `--source-url URL` can replace or accompany
the query. Omitted options use directory defaults. `--owner-id` may explicitly
select that same verified owner, but cannot select another account. Search uses
the engine at `--ipc-port` (or inherited `COMET_IPC_PORT`); updating the CLI alone
does not update an already-running engine's search behavior.

### Managed model choices

Crew's managed Codex picker uses the bundled shared model catalog, including
`gpt-6.1-sol`, rather than the native CLI's offline fallback. Codex receives the
bare model ID; Agent Auth still checks account/model access when a run starts.
Update and restart the running Crew engine to refresh these choices; replacing
only the frontend or CLI does not update its catalog behavior.

Managed account sign-in retries only temporary Agent Auth failures. A permanent
denial ends polling and shows an error; exchanged credentials remain available
until retry/cancel, and existing saved accounts are not removed. Canary profiles
use the existing staging pool; normal production profiles use production. An
explicit desktop-profile override follows that profile's deployment, not the
application bundle's display name.

### Engine and agent update recovery

Open Crew windows reconnect to a restarted engine only when its device and
workspace match. Reconnection restores observations while retaining the selected
session, unsent draft and message IDs; it never replays commands or mutations.

Background agent updates, including post-Crew-update refreshes, wait for existing
provider subprocesses without blocking new sessions or steering. Idle warm
processes can defer installation until their existing cleanup (up to 30 minutes).
Explicit **Update** requests retain priority over new runs. Existing global and
per-agent update preferences remain independent.

### Native Scaffold handoff

`comet session handoff [CHAT_ID] --prompt-file FILE` forks the last completed
native OMP turn, retaining history, attachments, source model and source context.
The default mode transfers the Git-visible worktree; its limits distinguish
25,000 changed entries from 50,000 materialized tracked files. New sandboxes use
the deployment's published source, not the controller's branch label.

Use `--session-only` to import native history into the existing published sandbox
checkout without capturing or replacing local worktree changes. Creation and
`--recover-chat-id ID --recover-sandbox-id ID` use dedicated session-only RPCs:
unsupported engines fail closed, never falling back to another creation or
worktree transfer. Recovery retains the immutable command, prompt and transfer mode.

For an interrupted handoff, retain the original command, prompt, database mode
and transfer mode, then name both already allocated targets:

```bash
comet session handoff CHAT_ID --command-id ORIGINAL_COMMAND --prompt-file FILE \
  --database-environment local --recover-chat-id TARGET_CHAT --recover-sandbox-id TARGET_SANDBOX
```

Recovery reuses confirmed allocation without replaying its ambiguous transfer.
It seals a separate recovery capture, publishes only an isolated workspace and
derived saved-history identity, then admits the original user command once.
Session-only recovery leaves the published checkout untouched. Pending captures
with missing or changed artifacts fail closed; later history never substitutes
for the saved fork. Update and restart the running Crew controller: updating its
CLI alone does not install this recovery behavior.
Isolated preparation requires a macOS or Linux controller; other platforms fail
before preparation without changing the existing handoff or deleting captures.

The native chat2 browser contract is `crew.native.web.v1`: verified assets install
under `.scaffold/crew/web`, runtime under `.scaffold/crew`, data under
`.scaffold/crew-data`, all relative to the sandbox API workspace (normally
`/workspace/ashler-platform`), with IPC port 39400. The supervisor waits for
`runtime-ready`; legacy session records are not reinterpreted as chat2.
Device bootstrap must match the configured project, optional pinned deployment
and Edge origin before the native host can attach.

Stopped/failed imported epochs remain immutable history, but their same public
session and deep link can explicitly **Start new epoch**. Desktop and mobile
require confirmation plus the observed `expectedLifecycleEpoch`; CLI callers use
`comet scaffold control --command-id ID --request-file FILE` with operation
`start`, `sandbox_id`, `scope`, and `expectedLifecycleEpoch`. This calls the
control plane's dedicated `/api/code-sandboxes/:id/start` transition, not Resume
or Attach. Unknown delivery is reconciled under the same command ID, never replayed.

Only its verified next-epoch response and fresh exact host authority can publish
the new association. The durable native-ID tombstone and runtime-cache reset
precede the epoch-specific release receipt; old history, config, source context
and the original import receipt remain intact. Refresh or an editable larger
epoch never authorizes this transition. Starting a host queues no agent prompt:
continuation requires a fresh user request and a new native conversation, never
resurrection of the destroyed original native process.

## Multi-device sync (optional)

Sign in to start an agent on one device and follow or drive it from another:

```bash
zeron daemon stop
zeron login        # or: zeron logout to return to local-only
zeron daemon start
```

Crew's Scaffold sign-in completes in the browser; `login` exits without an extra
keystroke. Hosted paste-code sign-in and workspace selection still use terminal input.

Devices signed in to the same account can read and write each other's workspace files, so only sign in devices you trust. Existing local sessions are never uploaded.

## Sponsors

Thank you to [The Context Company](https://www.thecontextcompany.com/) for sponsoring Zeron. You can help fund Zeron's development too by [becoming a sponsor on GitHub](https://github.com/sponsors/zeronsh).

---

Developing or curious how it works? [Ask DeepWiki](https://deepwiki.com/zeronsh/zeron) or check out [ARCHITECTURE.md](ARCHITECTURE.md).

Licensed under the [MIT License](LICENSE).
