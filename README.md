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

## Multi-device sync (optional)

Sign in to start an agent on one device and follow or drive it from another:

```bash
zeron daemon stop
zeron login        # or: zeron logout to return to local-only
zeron daemon start
```

Devices signed in to the same account can read and write each other's workspace files, so only sign in devices you trust. Existing local sessions are never uploaded.

## Crew Canary authentication

The Crew Canary Edge worker groups pending HTTP and WebSocket authorization checks by credential and deployment in the existing `AUTH_GRANTS` namespace. Each 200 ms rendezvous performs a fresh control-plane check; successful server authorization is never cached between batches.

Temporary authority failures share a fail-closed cooldown and preserve `Retry-After`. This does not change verification quotas, credentials, account routing, or device-grant storage.

Native Crew verifies user bearers at startup/sign-in, then treats them as non-expiring. Successful verification is retained until credentials change or an explicit authentication rejection invalidates the generation actually used. Concurrent revalidation is shared; stale responses cannot invalidate replacement credentials. There is no minute polling. An idle client may still display signed in until a server rejection arrives; server-side authorization remains enforced.

Device grants renew 30 seconds before their actual expiry, with shared renewal and failure backoff. Timeouts and temporary failures preserve credentials, profiles and pending sync data, but never authorize an unverified startup or return an expired device grant. Explicit `Retry-After` survives connectivity hints and late rejection feedback; rotating replacement credentials remain durable.

The native directory publisher retains pending jobs but stops querying an unchanged local snapshot after a server-version rejection, including across restarts. Transcript or registry progress enables a fresh lookup; server counters never become local publication evidence. Transient failures pause the worker for at least `Retry-After`.

Native inference shares an authority across compatible routes within one profile and credential generation. Acquisition and expiry renewal are coalesced; cancelling one waiter does not cancel another route's acquisition. Principal and scope remain pinned. Only an explicit Agent Auth authority rejection invalidates the matching grant; a later independent request may reauthorize, but the model request is never replayed. Provider 401 responses pass through unchanged. Renewal logs only authority digests, epochs and timestamps; a short successful inference turn is not evidence of expiry or renewal.

## Sponsors

Thank you to [The Context Company](https://www.thecontextcompany.com/) for sponsoring Zeron. You can help fund Zeron's development too by [becoming a sponsor on GitHub](https://github.com/sponsors/zeronsh).

---

Developing or curious how it works? [Ask DeepWiki](https://deepwiki.com/zeronsh/zeron) or check out [ARCHITECTURE.md](ARCHITECTURE.md).

Licensed under the [MIT License](LICENSE).
