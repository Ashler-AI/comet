# Crew on Zeron 0.2.127

Upstream baseline: `zeronsh/zeron` at
`31d26eeb6b9447cc6ef26e994f0dc62849014ab0` (v0.2.127).
Crew source before rebasing: `e36890e320674070a74cb959a366567856312e0a`.
The effective Crew delta is replayed onto upstream; the original history is
retained locally at `backup/crew-before-zeron-0.2.127-rebase`.

## Compatibility patches

- **Runtime and authentication:** retain Crew profile selection, scoped Scaffold
  authority, managed inference, Boat admission, and OMP. Pass the resolved
  runtime coordinates to the new background engine. Its executable also handles
  OMP supervisor and MCP entry points.
- **IPC and recovery:** use profile-bound IPC tokens for desktop, CLI, fixtures,
  and MCP. Keep one-shot mutation delivery, durable command admission and
  identity-preserving Codex resume/fork behavior while integrating upstream
  subscriptions, framing and recovery improvements.
- **Desktop:** retain Scaffold targeting, durable preparation/delivery state,
  managed account controls, and Crew settings alongside upstream transcript,
  browser, scrolling and settings improvements.
- **Mobile:** retain Crew login, account/profile isolation and push recovery;
  incorporate upstream terminals and companions. Generate Swift bindings from
  the combined Rust API rather than resolving generated ABI text manually.
- **Distribution:** ship `comet` plus `zeron-engine` on Linux and `comet` plus
  `Crew Engine` inside macOS bundles. Snapshot both executables under the build
  gate, sign nested macOS code with the environment's `.engine` identity, and
  reject incomplete packages before activation.
- **Edge:** retain Crew authorization, staging bindings and private release
  feeds. Incorporate chat attribution for preview ports and resolved input
  answers without adopting a new control plane.

## Deliberate exclusions

Upstream Cloud provisioning, credential vault, WorkOS login, billing, Android
application/publication and their deployment/demo scripts are outside this
integration. Crew continues to use its existing auth, Scaffold and release
systems. Shared upstream API types can remain where required by native/mobile
bindings; they do not authorize or expose those excluded services.

This source integration does not deploy, publish, migrate profiles, or change
Scaffold pins. Release qualification remains governed by
`skills/comet-work/SKILL.md`.
