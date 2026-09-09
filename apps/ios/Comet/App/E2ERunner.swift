// Headless e2e rig — launch with `-e2e` (plus a local wrangler dev edge and a
// `comet headless` engine in dev mode) and the app exercises the full live
// stack with no taps: workspace room backfill, device-relay RPCs, space/chat
// creation, the command plane, and session-room streaming. Results append to
// Documents/e2e.log for the harness to read via simctl.

import Foundation
import Loro

@MainActor
enum E2ERunner {
    static var logURL: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("e2e.log")
    }

    static func log(_ line: String) {
        let stamped = "[\(Int(Date().timeIntervalSince1970))] \(line)\n"
        print("E2E: \(line)")
        if let handle = try? FileHandle(forWritingTo: logURL) {
            handle.seekToEndOfFile()
            handle.write(Data(stamped.utf8))
            try? handle.close()
        } else {
            try? Data(stamped.utf8).write(to: logURL)
        }
    }

    static func run(model: AppModel) async {
        try? FileManager.default.removeItem(at: logURL)
        log("start")
        guard runSessionVisibility() else { return }
        model.signInDev(edgeURL: URL(string: "http://localhost:8787")!,
                        userId: "devuser", projectScope: "dev-org")

        // 1. Workspace room: wait for connection + the engine's device row.
        guard let workspace = model.workspace else {
            log("FAIL no workspace store")
            return
        }
        // Warm-start probe: rows visible BEFORE any network = disk hydration.
        log("warm-start devices=\(workspace.devices.count) chats=\(workspace.chats.count)")
        let device = await poll(timeout: 15, label: "workspace device") {
            workspace.connected ? workspace.devices.first { $0.platform != "ios" } : nil
        }
        guard let device else {
            log("FAIL workspace: connected=\(workspace.connected) devices=\(workspace.devices.map(\.id))")
            return
        }
        log("OK workspace synced; engine device \(device.id) (\(device.name))")

        // 2. Device relay: ListFolders on every engine device (stale rig
        // devices linger in the dev workspace doc — report each).
        var listing: FolderListing?
        for candidate in workspace.devices where candidate.platform != "ios" {
            do {
                let l = try await workspace.listFoldersDetailed(deviceId: candidate.id, path: nil)
                log("OK relay ListFolders[\(candidate.name)/\(candidate.id.prefix(8))]: \(l.path) → \(l.entries.count) entries")
                listing = l
            } catch {
                log("FAIL relay ListFolders[\(candidate.name)/\(candidate.id.prefix(8))]: \(error.localizedDescription)")
            }
        }

        // 2b. Live model catalog over the relay.
        let models = try? await workspace.listModels(deviceId: device.id, harness: "mock")
        log(models != nil ? "OK relay ListModels: \(models!.map(\.id))" : "FAIL relay ListModels nil")

        // 3. Space + chat + first run through the command plane (mock harness).
        let spaceId = await workspace.createSpace(deviceId: device.id,
                                                  path: listing?.path ?? "/tmp", gitDetected: false)
        log("space created \(spaceId)")
        // Relay-created spaces land via doc sync — eventually consistent.
        let space = await poll(timeout: 10, label: "space row sync") {
            workspace.spaces.first { $0.id == spaceId }
        }
        guard let space else {
            log("FAIL space row never synced")
            return
        }
        let chatId: String
        do {
            chatId = try await workspace.createChat(
                space: space,
                config: ChatConfig(harness: "mock", model: nil, reasoning: nil, sandbox: "workspace-write"))
        } catch {
            log("FAIL create chat: \(error.localizedDescription)")
            return
        }
        guard let chat = workspace.chats.first(where: { $0.id == chatId }),
              let store = model.sessionStore(for: chat) else {
            log("FAIL chat/session store")
            return
        }
        guard await store.sendRun(prompt: "e2e ping", chat: chat) else {
            log("FAIL run admission: \(store.sendFailure ?? "unknown")")
            return
        }
        log("run queued on \(chatId)")

        let entries = await poll(timeout: 30, label: "assistant reply") {
            store.entries.contains { $0.role == .assistant && !$0.parts.isEmpty } ? store.entries : nil
        }
        if let entries {
            log("OK transcript streamed: \(entries.count) entries")
        } else {
            log("FAIL no assistant reply; entries=\(store.entries.count) connected=\(store.connected) sendFailure=\(store.sendFailure ?? "none") pending=\(store.pendingSends.count)")
        }

        // 4. Big-doc backfill (fragmented): open the chat the seeder filled.
        let bigChatId = "e2e-big-doc"
        let bigChat = Chat(id: bigChatId, deviceId: device.id, title: "big", archived: false,
                           cwd: nil, branch: nil, checkoutId: nil, config: nil,
                           lastMessagePreview: nil, lastMessageAt: nil, createdAt: nowMs(),
                           harnessSessionId: nil, harnessSessionCwd: nil,
                           spaceId: spaceId, lastSeenAt: nil)
        if let bigStore = model.sessionStore(for: bigChat) {
            let big = await poll(timeout: 20, label: "big doc backfill") {
                bigStore.entries.count >= 40 ? bigStore.entries : nil
            }
            if let big {
                let bytes = big.flatMap(\.parts).reduce(0) { acc, part in
                    if case .text(_, let t) = part { return acc + t.count }
                    return acc
                }
                log("OK big-doc backfill: \(big.count) entries, ~\(bytes / 1024)KB text")
            } else {
                log("FAIL big-doc backfill: entries=\(bigStore.entries.count) connected=\(bigStore.connected)")
            }
        }

        log("done")
    }

    /// Isolated visibility regression: no edge, authority changes, or writes to
    /// the signed-in workspace. Uses the same accessors consumed by the UI.
    @discardableResult
    static func runSessionVisibility() -> Bool {
        let probe = AppModel()
        let space = Space(id: "visible-space", deviceId: "host", path: "/tmp",
                          gitDetected: false, createdAt: 0)
        func chat(_ id: String, spaceId: String?, archived: Bool, at: Int64) -> Chat {
            Chat(id: id, deviceId: "host",
                 title: "Crew visibility scenario", archived: archived,
                 createdAt: at, spaceId: spaceId)
        }
        let attached = chat("legacy-attached", spaceId: space.id, archived: false, at: 10)
        let detached = chat("AAAAAAAA-BBBB-4CCC-8DDD-EEEEEEEEEEEE", spaceId: nil, archived: false, at: 30)
        let missing = chat("legacy-missing", spaceId: "missing-space", archived: false, at: 20)
        let archived = chat("legacy-archived", spaceId: space.id, archived: true, at: 40)
        let archivedDetached = chat("BBBBBBBB-BBBB-4CCC-8DDD-EEEEEEEEEEEE", spaceId: nil, archived: true, at: 50)
        let archivedMissing = chat("legacy-archived-missing", spaceId: "missing-space", archived: true, at: 60)
        let rows = [attached, detached, missing, archived, archivedDetached, archivedMissing]
        probe.demo = DemoDataset(devices: [], spaces: [space], chats: rows, sessions: [:])
        let foreignIds = ["00000000-0000-4000-8000-000000000007", " legacy-membership ",
                          "CCCCCCCC-BBBB-4CCC-8DDD-EEEEEEEEEEEE", detached.id.lowercased()]
        let refIds = rows.map(\.id) + foreignIds
        for id in refIds {
            guard let first = probe.addSessionRef(id), first.chatId == id,
                  probe.addSessionRef(id) == first else {
                log("FAIL Crew demo membership: opaque ID changed or upsert duplicated")
                return false
            }
        }
        guard probe.overviewChats.map(\.id) == [detached.id, missing.id, attached.id],
              probe.settledChats.map(\.id) == [archivedMissing.id, archivedDetached.id, archived.id],
              Set(probe.sharedSessionRefs.map(\.chatId)) == Set(foreignIds),
              probe.sharedSessionRefs.count == foreignIds.count,
              probe.chats(in: space.id).map(\.id) == [attached.id] else {
            log("FAIL Crew session visibility: missing, duplicate, or misordered source")
            return false
        }
        // Import a real workspace snapshot, then use a normal membership write
        // to trigger production projection. No room, disk cache, or network starts.
        do {
            let userId = "principal:é"
            let config = AppConfig(edgeURL: URL(string: "http://127.0.0.1:1")!, mode: .dev,
                                   userId: userId, projectScope: "visibility-regression",
                                   deviceId: "viewer", deviceName: "Crew regression")
            let source = LoroDoc()
            let chatMap = source.getMap(id: "chats")
            let unrelated = [chat("other-principal-only", spaceId: space.id, archived: false, at: 90),
                             chat("unclaimed-project-row", spaceId: nil, archived: true, at: 100)]
            for chat in rows + unrelated {
                let row = try chatMap.getOrCreateContainer(key: chat.id, child: LoroMap())
                try row.insert(key: "id", v: chat.id)
                try row.insert(key: "deviceId", v: chat.deviceId)
                try row.insert(key: "title", v: chat.title ?? "")
                try row.insert(key: "archived", v: chat.archived)
                try row.insert(key: "createdAt", v: chat.createdAt)
                if let spaceId = chat.spaceId { try row.insert(key: "spaceId", v: spaceId) }
            }
            let statuses = source.getMap(id: "sessions")
            for chat in rows + unrelated {
                let row = try statuses.getOrCreateContainer(key: chat.id, child: LoroMap())
                try row.insert(key: "chatId", v: chat.id)
                try row.insert(key: "deviceId", v: chat.deviceId)
                try row.insert(key: "status", v: "working")
                try row.insert(key: "updatedAt", v: Int64(100))
            }
            let refs = source.getMap(id: "sessionRefs")
            for (index, id) in (refIds + ["other-principal-only"]).enumerated() {
                let owner = index < refIds.count ? userId : "other-principal"
                let row = try refs.getOrCreateContainer(
                    key: "\(owner.utf8.count):\(owner):\(id)", child: LoroMap())
                try row.insert(key: "userId", v: owner)
                try row.insert(key: "chatId", v: id)
                try row.insert(key: "addedAt", v: Int64(index))
            }
            source.commit()
            let workspace = WorkspaceStore(config: config)
            _ = try workspace.doc.importWith(bytes: source.export(mode: .snapshot), origin: "visibility-regression")
            // A valid UUID triggers projection even on the old UUID-filtered path.
            guard workspace.addSessionRef(chatId: foreignIds[0])?.chatId == foreignIds[0],
                  Set(workspace.sessionRefs.map(\.chatId)) == Set(refIds),
                  workspace.sessionRefs.count == refIds.count,
                  Set(workspace.sessions.keys) == Set(rows.map(\.id)),
                  workspace.overviewChats.map(\.id) == probe.overviewChats.map(\.id),
                  workspace.settledChats.map(\.id) == probe.settledChats.map(\.id),
                  Set(workspace.sharedSessionRefs.map(\.chatId)) == Set(foreignIds),
                  workspace.sharedSessionRefs.count == foreignIds.count else {
                log("FAIL Crew workspace projection: opaque IDs, principal isolation, or exact row/ref dedupe")
                return false
            }
            for (index, id) in refIds.enumerated() {
                guard workspace.addSessionRef(chatId: id) == SessionRef(chatId: id, addedAt: Int64(index)),
                      workspace.sessionRefs.count == refIds.count else {
                    log("FAIL Crew workspace membership: opaque ID, scoped key, or original timestamp changed")
                    return false
                }
            }
            workspace.rename(chatId: attached.id, title: "  Renamed\n Crew session  ")
            guard workspace.chats.first(where: { $0.id == attached.id })?.displayTitle == "Renamed Crew session" else {
                log("FAIL Crew workspace title: rename did not update the displayed title")
                return false
            }
            workspace.removeSessionRef(chatId: attached.id)
            guard !workspace.chats.contains(where: { $0.id == attached.id }),
                  workspace.sessions[attached.id] == nil,
                  workspace.doc.getMap(id: "chats").get(key: attached.id) != nil,
                  workspace.doc.getMap(id: "chats").get(key: unrelated[0].id) != nil else {
                log("FAIL Crew workspace membership: removal leaked status or deleted project data")
                return false
            }
            let environment = SessionEnvironment(
                source: SessionEnvironmentSource(kind: "scaffold", sandboxId: "visibility-sandbox"),
                ownerPrincipal: userId,
                scope: CollaborationScope(projectId: config.projectScope,
                                          deploymentId: "visibility-deployment", sessionId: "visibility-session"))
            let routedId = foreignIds[1]
            guard workspace.addSessionRef(chatId: routedId, environment: environment) != nil,
                  workspace.sharedSessionRefs.first(where: { $0.chatId == routedId })?.environment == environment,
                  workspace.addSessionRef(chatId: routedId) != nil,
                  workspace.sharedSessionRefs.first(where: { $0.chatId == routedId })?.deploymentId == "visibility-deployment",
                  workspace.sharedSessionRefs.first(where: { $0.chatId == routedId })?.environment == environment else {
                log("FAIL Crew workspace membership: environment routing lost during opaque-ID upsert or projection")
                return false
            }
        } catch {
            log("FAIL Crew workspace snapshot projection: \(error)")
            return false
        }
        // Legacy memberships remain visible; this does not authorize opening
        // their transcripts. The edge still rejects non-UUID session routes and
        // canonicalizes UUID routes independently of these exact document IDs.
        // Removing a space or opening an archived session cannot hide rows or
        // restore them implicitly. A vanished row must reveal its retained ref.
        probe.demo?.spaces = []
        if let row = probe.chat(id: archived.id) { _ = probe.sessionStore(for: row) }
        probe.markSeen(chatId: archived.id)
        guard probe.overviewChats.map(\.id) == [detached.id, missing.id, attached.id],
              probe.chat(id: archived.id)?.archived == true else {
            log("FAIL Crew session visibility: space removal or opening changed visibility")
            return false
        }
        probe.demo?.chats.removeAll { $0.id == missing.id }
        guard Set(probe.sharedSessionRefs.map(\.chatId)) == Set(foreignIds + [missing.id]) else {
            log("FAIL Crew session visibility: retained membership became unreachable")
            return false
        }
        probe.archive(chatId: detached.id)
        guard !probe.overviewChats.contains(where: { $0.id == detached.id }),
              probe.settledChats.contains(where: { $0.id == detached.id }),
              !probe.sharedSessionRefs.contains(where: { $0.chatId == detached.id }) else {
            log("FAIL Crew session visibility: archive lost or duplicated a session")
            return false
        }
        probe.restoreChat(chatId: archivedMissing.id)
        guard probe.overviewChats.first?.id == archivedMissing.id,
              !probe.settledChats.contains(where: { $0.id == archivedMissing.id }),
              probe.chat(id: archived.id)?.archived == true else {
            log("FAIL Crew session visibility: explicit restore changed the wrong sessions")
            return false
        }
        log("OK Crew session visibility: production snapshot and demo, opaque refs, principal isolation, exact row dedupe, archive, restore, recency")
        return true
    }

    @discardableResult
    static func runAttentionTransitions() -> Bool {
        let now: Int64 = 100_000
        var tracker = AttentionTracker()
        var chat = Chat(id: "attention", deviceId: "host", archived: false, createdAt: 0)
        func update(_ status: SessionStatus, at: Int64) -> [(String, String)] {
            tracker.update([chat.id: SessionRow(chatId: chat.id, deviceId: chat.deviceId,
                status: status, updatedAt: at)], chats: [chat], now: now)
        }
        guard update(.working, at: now - 10).isEmpty,
              update(.awaitingInput, at: now - 9).map(\.0) == [chat.id],
              update(.awaitingInput, at: now - 8).isEmpty,
              update(.working, at: now - 10).isEmpty,
              update(.awaitingInput, at: now - 7).isEmpty,
              update(.errored, at: now - 6).map(\.0) == [chat.id],
              update(.idle, at: now - 5).isEmpty,
              update(.working, at: now - 4).isEmpty,
              update(.idle, at: now - 3).map(\.0) == [chat.id],
              tracker.update([:], chats: [chat], now: now).isEmpty,
              update(.working, at: now - 4).isEmpty,
              update(.idle, at: now - 2).isEmpty else {
            log("FAIL Crew attention transitions: baseline, input, error, completion, heartbeat or replay")
            return false
        }
        // Advance from an older baseline: unlike replay, this stale transition
        // reaches the age guard, and the exact 45-second boundary stays fresh.
        tracker = AttentionTracker()
        guard update(.working, at: now - 60_000).isEmpty,
              update(.awaitingInput, at: now - 45_001).isEmpty,
              update(.errored, at: now - 45_000).map(\.0) == [chat.id],
              update(.working, at: now - 45_001).isEmpty,
              update(.errored, at: now - 44_999).isEmpty else {
            log("FAIL Crew attention freshness: stale transition, exact 45s boundary, or replay high-watermark")
            return false
        }
        chat.archived = true
        guard update(.awaitingInput, at: now).isEmpty else {
            log("FAIL Crew archived session alerted")
            return false
        }
        let model = AppModel()
        let config = AppConfig(edgeURL: URL(string: "http://127.0.0.1:1")!, mode: .dev,
                               userId: "attention", projectScope: "attention",
                               deviceId: "viewer", deviceName: "Crew regression")
        let workspace = WorkspaceStore(config: config)
        model.workspace = workspace
        tracker = AttentionTracker()
        chat.archived = false
        chat.title = "  Review\n session  "
        func titles(_ status: SessionStatus, at: Int64) -> [String] {
            update(status, at: at).map { chatId, _ in
                SessionNotifications.notificationTitle(
                    model.sessionTitle(for: chat, fallbackTitle: "Session \(chatId.prefix(8))"), chatId: chatId)
            }
        }
        guard titles(.working, at: now - 10).isEmpty,
              titles(.awaitingInput, at: now - 9) == ["Review session"] else {
            log("FAIL Crew attention title: visible session name")
            return false
        }
        chat.title = "Renamed session"
        guard titles(.errored, at: now - 8) == ["Renamed session"] else {
            log("FAIL Crew attention title: workspace rename did not reach the next alert")
            return false
        }
        let environment = SessionEnvironment(
            source: SessionEnvironmentSource(kind: "scaffold", sandboxId: "attention-sandbox"),
            name: "  Scaffold\n name  ", ownerPrincipal: config.userId,
            scope: CollaborationScope(projectId: config.projectScope,
                                      deploymentId: "attention-deployment", sessionId: chat.id))
        guard workspace.addSessionRef(chatId: chat.id, environment: environment) != nil,
              titles(.working, at: now - 7).isEmpty,
              titles(.idle, at: now - 6) == ["Scaffold name"] else {
            log("FAIL Crew attention title: Scaffold name must precede workspace rename")
            return false
        }
        workspace.removeSessionRef(chatId: chat.id)
        chat.title = " \n\t "
        let unicodeTitle = String(repeating: "\u{10400}", count: 120)
        guard titles(.awaitingInput, at: now - 5) == ["Session attentio"],
              SessionNotifications.notificationTitle(" \n\t ", chatId: chat.id) == "Session attentio",
              SessionNotifications.notificationTitle(unicodeTitle, chatId: chat.id) == unicodeTitle,
              SessionNotifications.notificationTitle(unicodeTitle + "x", chatId: chat.id)
                == String(unicodeTitle.prefix(119)) + "…",
              SessionNotifications.notificationTitle("New session", chatId: chat.id) == "New session" else {
            log("FAIL Crew attention title: blank fallback, explicit name, or Unicode truncation")
            return false
        }
        model.demo = DemoDataset(devices: [], spaces: [], chats: [chat], sessions: [:])
        model.demo?.sessionStore(for: chat.id).setEntries([
            MessageEntry(id: "private-prompt", role: .user,
                parts: [.text(id: "text", text: "Private prompt must stay in the transcript")],
                createdAt: now, deviceId: "host")
        ])
        guard model.sessionTitle(for: chat) == "Private prompt must stay in the transcript",
              titles(.errored, at: now - 4) == ["Session attentio"] else {
            log("FAIL Crew attention title: transcript-derived preview leaked into notification")
            return false
        }
        log("OK Crew attention transitions: input, error, working-only completion; baseline, heartbeat, stale age, exact 45s, replay high-watermark and archived suppression")
        return true
    }

    /// Uncertain admission must not duplicate a send when its reply is lost,
    /// even if the session becomes working or the message arrives later.
    static func runMobileParity() async {
        let config = AppConfig(edgeURL: URL(string: "http://127.0.0.1:1")!, mode: .dev,
                               userId: "parity-\(UUID().uuidString)", projectScope: "parity",
                               deviceId: "viewer", deviceName: "Crew regression")
        let store = SessionStore(chatId: "parity-retry", config: config)
        let chat = Chat(id: store.chatId, deviceId: "host", archived: false, cwd: "/tmp", createdAt: 0)
        var attempts: [SessionCommandPayload] = []
        store.commandSender = { payload in
            attempts.append(payload)
            throw RelayError.timeout
        }
        guard !(await store.sendRun(prompt: "keep this draft", chat: chat)),
              !(await store.sendSteer(prompt: "keep this draft")),
              attempts.count == 2,
              attempts[0].messageId == attempts[1].messageId,
              case .run = attempts[1],
              let messageId = attempts[0].messageId else {
            log("FAIL Crew retry changed identity or payload after lost admission reply")
            return
        }
        store.setEntries([MessageEntry(id: messageId, role: .user,
            parts: [.text(id: "text", text: "keep this draft")], createdAt: nowMs(), deviceId: "host")])
        guard await store.sendSteer(prompt: "keep this draft"), attempts.count == 2 else {
            log("FAIL Crew late materialization admitted a duplicate message")
            return
        }
        let metadata = SessionStore(chatId: "parity-metadata", config: config, metadataOnly: true)
        defer { metadata.stop() }
        do {
            let row = try metadata.doc.getList(id: "messages").pushContainer(child: LoroMap())
            try row.insert(key: "id", v: "first-user")
            try row.insert(key: "role", v: "user")
            try row.insert(key: "parts", v: LoroValue.fromJSON([["id": "text", "kind": "text", "text": "  First   user\n title  "]]))
            metadata.doc.commit()
            metadata.start()
            guard await poll(timeout: 5, label: "metadata title", { metadata.previewTitle == "First user title" ? true : nil }) != nil,
                  metadata.entries.isEmpty else {
                log("FAIL Crew metadata title required full transcript hydration")
                return
            }
            metadata.activateTranscript()
            guard await poll(timeout: 5, label: "transcript activation", { metadata.entries.first?.id == "first-user" ? true : nil }) != nil else {
                log("FAIL Crew metadata navigation failed to activate transcript")
                return
            }
            metadata.updateDeploymentId("another-deployment")
            guard metadata.entries.isEmpty, metadata.previewTitle == nil,
                  metadata.doc.getList(id: "messages").len() == 0 else {
                log("FAIL Crew deployment change reused another room's transcript")
                return
            }
        } catch {
            log("FAIL Crew metadata regression: \(error.localizedDescription)")
            return
        }
        log("OK Crew mobile parity: uncertain retry identity/payload, late materialization dedupe, metadata-only title, transcript activation, deployment isolation")
    }

    static func runStoreEviction() async {
        guard await AppModel.runStoreEvictionRegression() else { return }
        log("OK Crew store eviction")
    }

    private static func poll<T>(timeout: TimeInterval, label: String,
                                _ probe: @MainActor () -> T?) async -> T? {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if let value = probe() { return value }
            try? await Task.sleep(nanoseconds: 300_000_000)
        }
        log("timeout waiting for \(label)")
        return nil
    }
}

extension E2ERunner {
    /// Live-relay probe: runs inside the user's real signed-in session and
    /// interrogates every engine device — workspace presence, the device
    /// room's host attachment, and a real ListFolders with the exact error.
    @MainActor
    static func runLive(model: AppModel) async {
        try? FileManager.default.removeItem(at: logURL)
        log("live start edge=\(model.edgeURLString) mode=\(model.authModeRaw) user=\(model.storedUserId.prefix(18)) project=\(model.storedProjectScope.prefix(18))")
        let workspace = await poll(timeout: 25, label: "workspace connect") {
            model.workspace?.connected == true ? model.workspace : nil
        }
        guard let workspace else {
            log("FAIL workspace never connected: store=\(model.workspace != nil) "
                + "userId=\(model.storedUserId.isEmpty ? "EMPTY" : "set") "
                + "projectScope=\(model.storedProjectScope.isEmpty ? "EMPTY" : "set") "
                + "access=\(Keychain.load(key: "accessToken") != nil) "
                + "mode=\(model.authModeRaw)")
            return
        }
        log("devices: " + workspace.devices.map {
            "\($0.name)[\($0.platform)] id=\($0.id) presence=\(workspace.deviceOnline($0.id))"
        }.joined(separator: ", "))
        guard let config = model.diagnosticsConfig else {
            log("FAIL no config")
            return
        }
        for device in workspace.devices where device.platform != "ios" {
            let status = await config.deviceStatus(deviceId: device.id)
            log("\(device.name) /status → \(status)")
            do {
                let listing = try await workspace.listFoldersDetailed(deviceId: device.id, path: nil)
                log("OK \(device.name) ListFolders → \(listing.path) (\(listing.entries.count) entries)")
            } catch {
                log("FAIL \(device.name) ListFolders → \(error.localizedDescription)")
            }
        }
        log("done")
    }
}
