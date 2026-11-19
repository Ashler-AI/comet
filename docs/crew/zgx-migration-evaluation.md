# Crew on zgx: evaluating upstream's new desktop app and how to get there

*Evaluation, 2026-11-18. Nothing was built, pushed or deployed. Every merge figure below
comes from `git merge-tree`, which writes no refs or files.*

| Input | Commit |
| --- | --- |
| Upstream `zeronsh/zeron` main | `b1064c1bd` (2026-11-18), last tag v0.3.8 |
| Crew `Ashler-AI/comet` main | `1932df84d` (0.2.151) |
| Existing thin port `crew/zgx-thin-port` (local only) | `e88ddb893` (2026-11-13), plan in [zgx-thin-port.md](zgx-thin-port.md) |

## 1. Recommendation

1. **Migrate.** Upstream deleted its GPUI app (`crates/ui`, about 267k lines) on 2026-11-17
   (zeronsh/zeron#1499). Every upstream change from here targets the zgx app and an engine Crew
   does not run. Staying on GPUI means owning that UI alone, behind an engine gap that grows by
   roughly 40 merged upstream changes a week.
2. **Finish the thin port; do not merge upstream into Crew main.** Measured today:
   - Upstream main into Crew main: 264–402 conflicted paths. 447–1,269 non-UI hunks
     (23k–33k conflicted lines), depending on which base is honest (§4). Every Crew UI feature
     must still be rebuilt in Luau afterwards. Crew's changes stay inside hot upstream files, so
     the next update conflicts again.
   - Upstream main into `crew/zgx-thin-port`, after 233 upstream commits and five days: 19
     conflicts. Ten are content conflicts, 18 hunks and 273 lines. The other nine are deletions
     the port already intends.
   - The thin port starts from upstream and adds Crew in Crew-named files with small seam edits:
     102 files, touching 38 upstream files. That shape stays cheap to update. The Crew-only
     modules it still lacks (§7) can mostly be copied over and rewired through upstream's hooks.
3. **Keep production on GPUI Crew 0.2.15x until the zgx build passes the same acceptance gates.**
   - Production Scaffold moved to native Crew only on 2026-11-16, and the production channels
     went to 0.2.150 the same day.
   - Freeze new Crew features on `crates/ui`: build them on the port. Fix only bugs there.
   - Cherry-pick upstream engine fixes only when production needs them.
4. **Sequence** (§8):
   1. Bring the port current.
   2. Finish the sign-in / Edge / zgx Crew-mode gates.
   3. Agent Auth and OMP.
   4. Scaffold, including a runtime archive Platform admits.
   5. Crew UI features as built-in plugins.
   6. Channels, updater and packaging.
   7. Canary, then Staging dogfood, then the production cutover.
   8. Weekly upstream merges.
5. **Effort (inferred):** about 80–130 engineer-days of implementation, plus Platform and
   release-gate time. §9 has the breakdown and what would shrink it.
   - Most of it is engine work, not UI. zgx cannot run on Crew's engine: it needs about 70
     methods Crew lacks.
   - Crew's own desktop UI is only about 2.5k production lines over existing engine methods.

## 2. What upstream changed

| Date | Upstream | Meaning for Crew |
| --- | --- | --- |
| 2026-10-15 | `3b2f78027` (v0.2.113+2) | Crew's git merge base |
| 2026-10-28 | `a0468502a` (v0.2.129+1) | Crew's effective base: Crew #152 squash-imported a branch rebased here, so ancestry was lost |
| 2026-11-03 | v0.2.136: `apps/zgx` lands in main as a beta | |
| 2026-11-12 | v0.3.0: the zgx app is the shipped desktop app | |
| 2026-11-13 | `94ae5b108` (v0.3.4+39) | Base of `crew/zgx-thin-port` |
| 2026-11-17 | v0.3.8, the last tag with `crates/ui`; then #1499 deletes it | |

**The framework.** zgx (`github.com/zeronsh/zgx`) is public and MIT.
- It is young and fast:
  - created 2026-09-26;
  - about 950 commits to `main` in the last 14 days, most by an automation account;
  - 173 open issues.
- Upstream pins it by git revision: 112 `Cargo.lock` changes since 11-04, across 63 distinct revisions.
  The current pin is on a zgx branch that is 217 commits behind zgx's `main`.
- It renders through wgpu with patched `winit`, `wgpu-hal` and `accesskit`. Every consumer
  repeats those patches, which is why `apps/zgx` is its own Cargo workspace.

**The app** (`apps/zgx`):
- **Code:** a Rust host of about 71k lines (windows, fonts, actions, engine client) and a Luau
  interface (`ui/`, 671 files, about 258k lines) that hot-reloads as it is saved.
- **Platforms:** macOS 13+ on Apple silicon only (Ghostty draws the terminals and needs Zig 0.16
  to build), Linux x86_64 and aarch64, Windows x86_64 (unsigned).
- **Releases:** v0.3.0 to v0.3.8 is nine releases in six days.
- **Tests:** 792 Rust unit tests, 56 integration tests and 185 Luau test files, run by
  `zgx-tests.yml`.

**The engine contract.**
- The protocol is unchanged: JSON `{id, method, params}` over a loopback WebSocket with the
  `ipc-token` bearer. crew-web already speaks it.
- Upstream's engine has 258 methods. 98 of them are not in Crew's engine, about 70 of which the
  zgx app calls by name.
- Release builds require the `zgx-interface-v1` capability.
- **zgx cannot run on Crew's 0.2.151 engine.** Adopting zgx therefore means rebasing Crew's
  engine onto upstream, whichever strategy is chosen.

**The extension seams** (`apps/zgx/docs/{plugins,templates}.md`):
- **Built-in plugins** (`apps/zgx/plugins-builtin/`) are unsigned, granted every permission they
  ask for, and can only be switched on or off. Upstream's own Git and GitHub features are built
  this way. A plugin can add:
  - a sidebar page, panel tabs, controls in the session header, composer buttons and row menus;
  - agent tools and notifications, and new chats on a chosen device;
  - a declarative settings page.

  A plugin's `service.luau` runs inside the engine on each device. Its operations are a fixed
  set (`run`, `fetch`, `folders`, `notify`, `agents`, store/emit/watch), and there is no
  passthrough to arbitrary engine RPCs.
- **Templates** overlay whole files and can't touch `account/`, onboarding or consent. Keeping
  one in step is manual: upstream's own Zeron v2 template needed 127 "in step" commits since
  11-04. They are not a vehicle for Crew.
- **Only host or `ui/` edits reach:** identity and channels, sign-in, onboarding, the account
  stores (including `account/cloud.luau`), the consent window, the Settings shell, About &
  updates, the menu bar, deep links and the engine manager.

## 3. Where Crew is

- **Production:**
  - The desktop app was already native (GPUI) Crew.
  - Production Scaffold moved to native on 2026-11-16, and the production desktop and Scaffold
    channels have been at 0.2.150 since that day (#222, #223).
  - Retiring the legacy Worker is still pending.
- **Fork shape:**
  - 803 Crew commits on the base.
  - Upstream content imported only by squash and cherry-pick: the effective base is `a0468502a`.
  - Crew's `crates/ui` delta is 157 files, +46.9k/−6.3k lines. All but about 2.5k production
    lines of it is upstream code from #152 (§6.2).
- **Crew-only contracts that a new engine must keep:**
  - 26 engine methods. Among them: `ReadCommand`, `ScaffoldHostAuthority`, the handoff/recover
    methods, `ResolveCrewSessionLink`, the annotations methods, `UpdateAccessToken`,
    `WakeDevice`, the `scaffold.*` and `crew.*` namespaces.
  - crew-web's `crew.native.web.v1` contract (`apps/web/contract.json`).
  - The Scaffold runtime contract `scaffold.comet-runtime.v2`.
  - The iOS app.
- **Channels:** one table, `apps/zeron/src/environment.rs`. Bundle ids are rewritten per channel
  by `scripts/package-macos.sh`.

  | Channel | Data dir | IPC port | Bundle id |
  | --- | --- | --- | --- |
  | Crew | `.crew` | 27958 | `ai.ashler.comet` |
  | Crew Staging | `.crew-staging` | 27957 | `ai.ashler.comet.staging` |
  | Crew Canary | `.crew-upstream-canary` | 27954 | `ai.ashler.crew.upstream-canary` |
- **Verification capacity:**
  - No CI test lanes since #156.
  - On the development Mac, local Rust builds are serialized across agents, and the data volume
    is 95% full (48 GB free). Those disk limits paused the thin port.
- **The thin port** (`docs/crew/zgx-thin-port.md`):
  - Slice 0 (baseline) is done.
  - Slices 1–4 are coded:
    - isolation and the Crew flavour;
    - Scaffold sign-in in the engine;
    - Crew's authority on upstream's Edge;
    - "zgx Crew mode" (account calls answered locally, a Crew onboarding path, Crew Settings
      account page).
  - The Edge and zgx slices are unit-tested. The engine slice was not yet built when work
    paused. No slice has passed its exit gate.
  - Slices 5–7 have not started: Agent Auth, Scaffold, packaging.
  - It deliberately isolates itself from every real Crew install: own data dir and ports, and it
    refuses Crew's.

## 4. Measurements

| Trial merge | Base | Conflicts | Non-UI content hunks / lines | UI |
| --- | --- | --- | --- | --- |
| upstream main → Crew main | `3b2f78027` (git) | 402 | 1,269 / 32.9k | 115 modify/delete in `crates/ui` |
| upstream main → Crew main | `a0468502a` (effective) | 264 | 447 / 23.3k | 115 modify/delete |
| v0.3.8 → Crew main | `3b2f78027` | 328 | 1,288 | 265 content hunks in `crates/ui` (49 with the effective base) |
| upstream main → `crew/zgx-thin-port` | `94ae5b108` | 19 | 18 / 273 | none; plus 8 workflow deletions and `crates/ui/src/state.rs` |

**Reading the Crew-main rows:**
- About 65% of the git-base hunks are the same upstream change landing twice. 73 files can be
  scripted as "take upstream".
- About 50 Crew files kept pre-import code. A merge with the effective base silently drops
  upstream's v0.2.113→v0.2.129 edits in 9 of them (`edge/src/auth.ts`, `preview-route.ts`,
  `diff_sync.rs`, `auth_cli.rs` among them).
- A merge with the git base silently brings back everything Crew excluded on purpose:
  - `edge/src/cloud` and `edge/vault`;
  - `apps/android` and `workos.ts`;
  - 15 CI workflows.

**Where the remaining conflict concentrates** (effective-base hunks):

| Cluster | Hunks | Difficulty |
| --- | --- | --- |
| Sessions / run lifecycle / RPC: Crew's execution fence and imported recovery against upstream's warm starts, revivals and 30 s idle stop | 171 | High |
| Harness launch: Agent Auth environment against upstream's prewarm, access/plan modes and update manager | 71 | High |
| Updater and managed engine | 45 | High |
| Edge authorization | 48 | High |
| Auth and accounts | 22 | Medium-high |
| iOS | 24 | Medium |

## 5. Options

| Option | What it means | Verdict |
| --- | --- | --- |
| A. Stay on GPUI | Keep `crates/ui`; cherry-pick engine fixes | **No.** Cheapest this month, but upstream no longer tests GPUI against its engine, and zgx-era features (plugins, the 98 new methods) never reach Crew. |
| B. Merge upstream main into Crew main, then rebuild the UI | One large merge, then zgx | **No.** 40–60 engineer-days just to land the merge (inferred). The UI rebuild comes on top, and the next merge is large again. |
| C. Merge v0.3.8 first, keep shipping GPUI, take zgx later | Engine catch-up while GPUI keeps shipping | **Only as a fallback.** Same 40–60 days, and GPUI is kept alive on an engine upstream stopped testing it with. Worth it only if production needs the new engine before the zgx build is ready. |
| D. Finish the thin port | Start from upstream; Crew at the seams | **Yes.** About a day to bring current. The remaining work is porting Crew modules, which any option needs. |

## 6. Porting Crew's features onto zgx

### 6.1 Where each kind of change goes

| Kind of change | Mechanism | Examples |
| --- | --- | --- |
| New surfaces and controls | Built-in `crew-*` plugin, plus a `crew` operation in the engine's plugin-service layer where it needs Crew engine calls (engine-side edit only) | Scaffold page and controls, devbox, sandbox recovery, annotations panel, shared-account view |
| Identity, auth, onboarding, About & updates, deep links, menus | Crew-named host files (`apps/zgx/src/crew*.rs`) and small `ui/` edits, behind a Crew channel setting in place of upstream's `beta` | Channel table, Crew URL schemes (`comet:`, `comet-staging:`), Crew sign-in and Settings account page |
| Behaviour in the engine | Upstream hooks rather than threading Crew types through upstream structs | `RunEnvironment::prepare` for Agent Auth; a Crew adapter behind the Cloud RPCs for Scaffold |
| Colours | A theme (data only) | Crew palette |
| Wording | Locale overlay | 142 messages in `ui/locales/en.json` name Zeron; about 50 `zeron.sh` links in `ui/` |
| Templates | Not used | Whole-file copies age worse than git patches |

**Scaffold through the Cloud row.** zgx's Cloud UI already does create, attach, send, stream,
interrupt, sleep, wake and retry against the engine's Cloud RPCs. A Crew adapter behind those
RPCs gives a first version with no new Luau.
- **It lacks:** stop/terminate, recovery, handoff, devbox, a project/deployment picker, and
  more than one Cloud per account. These go in a `crew-scaffold` plugin.
- **To hide:** credits, machine size and Billing. They live in `account/` and `settings/`,
  which needs `ui/` edits.

### 6.2 Feature map

Crew's own desktop UI is small.
- Only about 2.5k non-blank production lines (plus about 1.5k test lines) in `origin/main`'s
  `crates/ui` appear in no upstream tree.
- The other 96% of its 46.9k-line diff from the git base is upstream code imported by #152.
- Every feature below runs on engine methods Crew already has. **The port is engine work and
  identity work; the Luau part is modest.**

| Crew feature | What users see | Prod lines today | zgx home |
| --- | --- | --- | --- |
| Run on Scaffold from the composer | A "Scaffold (Local/Staging/Production DB)" target; a placeholder session (Creating → Starting → Connecting); the first prompt sent exactly once; the draft kept on failure | ~470 (+~300 test) | Cloud row for v1; then a `crew-scaffold` composer control. Consider reusing `crates/client/src/crew_launch.rs` (1,669 lines, the iOS journaled flow behind `LaunchPort`) from the host (inferred) |
| Settings › Sandboxes | Lifecycle list. Create, use, resume, pause, new epoch, apply model/account, inspect, stop. Controller-only | ~340 | `crew-scaffold` plugin page plus an engine service operation, modelled on zgx's Cloud screen |
| Scaffold session surfaces | "sandbox · lifecycle"; owner/project/deployment tooltip; Web/Tilt links; tab label | ~130 | Plugin at `#session-header` and a panel tab |
| Shared accounts (Agent Auth pool) | One row per login, usage windows, Remove / Remove copies, Add via browser plus pasted code | ~240 | `ui/` seam in Providers/Accounts. Upstream pins and switching are refused in managed mode |
| Managed-mode gating | Sign-in required to send; only OMP, Claude Code, Codex and Antigravity; voice off; managed thread naming | ~55 plus flags | Host flag plus small `ui/` edits, paired with Agent Auth failing closed |
| Crew deep links | Opens Crew session and conversation links, kept through sign-in (30 min); Copy link | ~125 | Host links seam. Fix the scheme bug first (§12) |
| Crew update channel | Private authenticated feed; Crew update prompts | ~35 plus `crates/update` | `about.rs` plus the update adapter (phase 5) |
| Branding and sign-in copy | Crew names and logo, ~68 strings | ~60 | Channel table plus locale overlay (phase 5) |
| Gemini/Antigravity effort grouping | One row per model; effort maps to model variants | ~45 | Move into the engine's `ListModels` rather than the picker |
| Devbox (Boat/Namespace) | Wake; Add devbox; Remove offline device; Idle labels; wake-on-send and resend once (570 s budget) | ~550 (+~400 test) | Plugin plus service operation, plus a hook in the send path. Crew's `deleteDevice` becomes upstream's `removeDevice` |
| Shared annotations | Create, edit and resolve notes on a message or a selection | ~450 | Plugin, which needs a message-selection API; or retire with an export. It was only ever smoke-tested |
| Permission diagnostics | PID and executable path on Appshots | ~11 | Trivial, or drop |

**Also needed:** hide what zgx ships and Crew excludes.
- Cloud credits, machine size and Billing.
- WorkOS organizations and Members.
- Username and member-card onboarding.
- Zeron's intro and `zeron.sh` links.

**Already upstream, so don't port:**
- pinned sessions, project actions and the right-sidebar file tree;
- PR badges (the built-in GitHub plugin);
- the model catalog additions, which are engine-only (#198, #225).

**Moot on zgx, but re-check the behaviour:** Crew's 18 performance and fix commits to GPUI code,
including the sidebar spinner rebuild. On zgx:
- Is opting into list row deltas needed?
- Is the update install off the UI thread (`apps/zgx/src/about.rs`)?
- Do the "Preparing workspace" and delivery-failure states exist?
- Do web links default to the system browser (#194)?

### 6.3 Method

- Port behaviour, not screens. Each Crew feature gets one acceptance check in the headed zgx app
  (`--inspect` with `zgx inspect` drives it from an agent).
- Upstream equivalents win: comments, settings pages, PR badges, terminals, browser, themes.
- A feature with no Crew users since the native cutover is a candidate to drop. Say so
  explicitly in the release scope rather than porting it by default.

## 7. Backend the thin port still has to carry

Sized in Crew main today:

| Feature | Files / LOC | Lands on |
| --- | --- | --- |
| Scaffold lifecycle, handoff, recovery, native import (`scaffold.rs` alone is 7,502 lines) | 11 / 17.7k | Crew adapter behind the Cloud RPCs; `crew-scaffold` plugin; sandbox-host runtime |
| Import and backup | 9 / 8.2k | Candidate to retire now that production is native; owner decision |
| OMP harness | 7 / 5.9k | A Crew harness beside upstream's |
| Agent Auth relay and run context | 6 / 3.5k | `RunEnvironment::prepare`; fail closed on every other inference path (§10) |
| Deep links and mobile FFI | 4 / 2.7k | Host links module, `crates/mobile` |
| Annotations | 2 / 2.0k | Engine methods plus a plugin |
| Devbox / Boat | 4 / 1.9k | Plugin plus a service operation |
| crew-web and its 2 Crew-only methods | 9 / 1.9k | Unchanged contract; needs `ReadCommand` and `ScaffoldHostAuthority` in the new engine |
| Updater (Crew modules plus edits to `update/lib.rs` and `managed.rs`) | 4 / 1.1k | An adapter inside upstream's update transactions (§10) |
| iOS | 3 Crew-only files / 0.8k, plus +2.4k/−0.9k in 27 upstream files | Upstream iOS plus Crew deltas; regenerate the bindings |

## 8. Plan

Every phase keeps the thin port's isolation rules until the cutover: its own data dirs and
ports, and no pushes, CI or deploys without approval.

| # | Phase | Exit gate |
| --- | --- | --- |
| 0 | Bring `crew/zgx-thin-port` to upstream main (19 conflicts); record the pins. Move builds to CI or a devbox. | Port builds on current upstream; owner decisions in §11 taken |
| 1 | Finish slices 1–4: isolation, Scaffold sign-in, local Edge, zgx Crew mode | The thin port's headed check: sign in, onboard, open the main window, run a local agent turn |
| 2 | Agent Auth through `RunEnvironment`; OMP; managed model catalog | Real Claude Code, Codex and OMP turns route through Agent Auth with receipts. Prewarm, the Auto reviewer, pins, account switching, voice and model probes fail closed |
| 3 | Scaffold: Cloud-row adapter, then a `crew-scaffold` plugin for stop/recover/handoff. A sandbox-host runtime on the new engine; crew-web unchanged | Staging Platform admits the new runtime archive. Create → stream → pause → resume (same id) → stop, with controller restarts and lost responses |
| 4 | Crew UI features (§6.2) as `crew-*` plugins and host seams | Each feature's acceptance check passes in the headed app |
| 5 | Distribution: Crew channel table in `crates/cli`; a channel-aware packager (bundle id, scheme, icon, engine identifier, artifact names); the authenticated per-channel feed inside upstream's update transactions; CI with Zig 0.16, a Ghostty cache and a zgx mirror; the Linux headless archive | A signed Canary bundle installs beside Crew Staging and updates itself |
| 6 | Cutover readiness. Order: an Edge that accepts both 0.2.15x and new clients, then the Scaffold runtime, desktop and iOS. Upgrade the existing `.crew*` profile in place, carrying Crew-only stores. Validate or re-vendor the loro patch | Copied-profile upgrade loses nothing. Mixed-version rooms work. Rollback to GPUI Crew is rehearsed |
| 7 | Crew Canary, then Crew Staging dogfood, then production through the existing release gates | Captain verification on Staging; production only with explicit approval |
| 8 | Steady state | Weekly upstream merges (real merges, ancestry kept). zgx moves only through upstream's pin. A lean CI lane runs the zgx and engine suites |

Phases 2, 3 and 5 can run in parallel once phase 1's contracts are fixed. Phase 4 can start
alongside phase 2 for features that don't depend on Scaffold.

## 9. Effort (inferred)

| Work | Engineer-days |
| --- | --- |
| Phases 0–1: bring current, finish and verify slices 1–4 | 5–8 |
| Phase 2: Agent Auth, OMP, model catalog, fail-closed coverage | 12–18 |
| Phase 3: Scaffold adapter, plugin, sandbox runtime, crew-web | 20–30 |
| Phase 4: Crew UI features (§6.2): Run on Scaffold 4–6, Sandboxes page 3–5, devbox 3–5, annotations 3–4, the rest 2–10 | 15–30 |
| Phase 5: channels, packaging, updater, CI | 10–15 |
| Phase 6: Edge compatibility, profile upgrade, loro, iOS | 12–20 |
| Phase 7: staged rollout and fixes | 5–10 |
| **Total** | **≈80–130** |

That total is consistent with the backend-only estimate for finishing the thin port (45–70 days
across phases 2, 3 and 6). Calendar time depends on how many streams run in parallel (§8). It
also depends on Platform admitting the new runtime and on the release gates. Neither can be
compressed by adding engineers.

**What shrinks it:**
- Retire legacy import/backup (8.2k lines) and anything else unused since the native cutover.
- Accept the Cloud row for Scaffold v1.
- Ship without annotations or devbox if they have no current users.
- Reuse the iOS journaled launch flow (`crates/client/src/crew_launch.rs`) for Run on Scaffold
  instead of rebuilding it.

**What grows it:**
- Platform admission of a new runtime archive.
- Upstream reshaping the run lifecycle again during the port.
- No CI test lanes.

## 10. Risks

1. **Run-lifecycle semantics.** Crew's execution fence and imported recovery must hold against
   upstream's warm starts, boot revivals and 30 s idle stop, or Crew gets duplicate or stopped
   writers. Port Crew's fault cases (lost acknowledgements, restarts, stale commands) as tests
   first.
2. **Churn.**
   - Upstream shipped nine releases in six days, and the thin port fell 233 commits behind in
     five.
   - The zgx pin sits on a side branch of zgx.
   - Mitigation: merge weekly, never move zgx independently, and keep Crew code in Crew files.
3. **Agent Auth coverage.** Upstream added inference paths (prewarm/warm start, the Auto
   reviewer, account pins and switching, API keys, voice, model probes). Each must route
   through Agent Auth or fail closed in managed mode. `RunEnvironment` has no run-ended
   callback, so routes retire when replaced or at shutdown.
4. **Edge authorization and mixed versions.**
   - New lanes need Crew's authority checks: `checkpoint-part`, attachment blobs, `chat-host`,
     caller-device stamping and the relay layer.
   - Upstream's registry is built for mixed versions: unknown kinds are rows older clients
     ignore. But Crew's Edge validates columns (#218), so it must admit the new kinds before
     any new client connects.
5. **Updater.** Upstream's update trial proves the network with an unauthenticated GET of
   `/releases/latest.txt`. Crew's Edge requires a session for `/releases/*`, so a signed-in
   trial that never connects would read as "no network" and be committed. Plug Crew's
   authenticated feed into the transaction framework instead.
6. **Scaffold runtime.** Sandboxes run Platform's pinned `comet headless`. A new desktop needs a
   new-engine runtime archive that Platform admits, coordinated with the Edge and the pins.
7. **loro.** Crew carries a patched `loro-internal` 1.13.9. The patch has two parts: a read-only
   inspection API for migration, and a serialized FIFO that keeps local updates causally
   ordered across reentrant and concurrent commits. Upstream is on loro 1.17. Check that 1.17
   fixes the ordering bug before dropping the patch.
8. **Seam upkeep.** Upstream keeps adding account features that call its Edge directly
   (usernames, cards, photos, members). The thin port's fail-closed `Auth::bearer` seam must
   stay, and each new call needs a Crew answer.
9. **Platform floor.** Apple silicon and macOS 13+ only. `ui/` ships as readable Luau source in
   the bundle.
10. **Capacity.** The development Mac's disk is nearly full and its builds are serialized.
    Crew has no CI test lanes, while the zgx app's safety net is upstream's 1,000-odd tests.

## 11. Decisions for the owner

1. Strategy: make the thin port the destination (option D), with option C only as a fallback.
2. Agent Auth through upstream's `RunEnvironment` rather than `RunControls`. The latter means
   58 struct literals in 39 files that conflict on every update.
3. Codex resume failure: Crew's hard error, or upstream's silent fresh conversation.
4. Scaffold authority for the port's test sign-in: staging Platform, or the old canary.
5. Scope at cutover: retire legacy import/backup? Ship without annotations or devbox if unused?
   Accept the Cloud row as Scaffold v1?
6. Approvals:
   - push `crew/zgx-thin-port` to `origin`;
   - a build-only CI lane with Zig on Ashler-AI/comet;
   - a test Edge;
   - a Crew Canary zgx channel;
   - optionally, a lean CI test lane.
7. Timing: start after the native production cutover settles (legacy Worker retired), or now on
   the isolated lane.
8. Confirm that no Crew user needs an Intel Mac or macOS 12.

## 12. Found along the way: two bugs in shipped Crew

Both are verified in source and in the installed bundles: `Crew.app` 0.2.148 (production
identity) and `Crew Staging.app` 0.2.151. Both are independent of this migration.

1. **Every build is branded "Crew Canary".** The string is hard-coded with no channel setting:
   - the app menu ("About/Hide/Quit Crew Canary": `crates/ui/src/app_menus.rs:177-198,405`);
   - the Windows title (`lib.rs:463`);
   - the update prompts ("Crew Canary 0.x is available": `shell.rs:10083-10133`);
   - the Appshots text (`settings/appshots.rs:11`).
2. **Copied links only open in Canary.**
   - `crew_scope_link` and `zeron_conversation_link` always write `crew-upstream-canary://`
     (`crates/ui/src/links.rs:68,81`).
   - Production registers `comet:` and Staging `comet-staging:` (`dist/README.md`).
   - The conversation-link parser accepts only `crew-upstream-canary://` and `zeron://`.

   So a link copied in production or Staging opens only where Crew Canary is installed.
