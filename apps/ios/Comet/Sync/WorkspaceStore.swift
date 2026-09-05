// Workspace doc mirror. The edge binds this app to its verified project room;
// rows are globally shared within that project while `sessionRefs` are scoped
// to the authenticated principal. iOS is a viewport, not an engine device, so
// it deliberately owns neither a device row nor a presence heartbeat.

import Foundation
import Loro
import Observation

@MainActor
@Observable
final class WorkspaceStore {
    private(set) var devices: [DeviceRow] = []
    private(set) var spaces: [Space] = []
    private(set) var chats: [Chat] = []
    private(set) var sessions: [String: SessionRow] = [:]
    private(set) var sessionRefs: [SessionRef] = []
    private(set) var presence: [String: Int64] = [:]  // deviceId → last heartbeat ms
    private(set) var connected = false

    let doc = LoroDoc()
    private var room: RoomClient?
    private var subscriptions: [Subscription] = []
    private let config: AppConfig

    init(config: AppConfig) {
        self.config = config
    }

    @ObservationIgnored private var saver: DocSaver?

    func start() {
        guard room == nil else { return }
        let roomId = "ws4/\(config.projectScope)"
        // Local-first: hydrate from the on-device snapshot before joining —
        // the sidebar renders immediately and the join backfills incrementally.
        if DocDisk.load(into: doc, id: roomId) {
            project()
        }
        saver = DocSaver(docId: roomId, doc: doc)
        let client = RoomClient(roomId: roomId, doc: doc) { [config] in
            await config.workspaceSocketURL()
        } events: { [weak self] event in
            Task { @MainActor [weak self] in self?.handle(event) }
        }
        room = client

        // Local commits → room. The subscription fires synchronously inside
        // commit; hop to the actor to send.
        let localSub = doc.subscribeLocalUpdate { [weak client, weak self] update in
            guard let client else { return }
            let bytes = [UInt8](update)
            Task { await client.sendLocalUpdate(bytes) }
            Task { @MainActor [weak self] in self?.saver?.poke() }
        }
        subscriptions.append(localSub)

        Task { await client.start() }
        project()
    }

    /// Backgrounding hook: persist immediately.
    func flushToDisk() {
        saver?.flush()
    }

    func stop() {
        subscriptions.removeAll()
        saver?.flush()
        if let room {
            Task { await room.stop() }
        }
        room = nil
        connected = false
    }

    private func handle(_ event: RoomEvent) {
        switch event {
        case .connected:
            connected = true
            purgeLegacyMobileDevices()
            project()
        case .disconnected:
            connected = false
        case .remoteUpdate:
            purgeLegacyMobileDevices()
            project()
            saver?.poke()
        case .ephemeralUpdate:
            projectPresence()
        }
    }

    /// Older iOS builds registered themselves as engine devices. Mobile is a
    /// controller only: remove those synced rows so desktop device pickers do
    /// not retain simulator/phone model names forever.
    private func purgeLegacyMobileDevices() {
        guard let root = doc.getDeepValue().mapValue,
              let deviceRows = root["devices"]?.mapValue else { return }
        let staleIds = deviceRows.compactMap { id, value -> String? in
            value.mapValue?["platform"]?.stringValue == "ios" ? id : nil
        }
        guard !staleIds.isEmpty else { return }
        let map = doc.getMap(id: "devices")
        do {
            for id in staleIds {
                try map.delete(key: id)
            }
            doc.commit()
        } catch {
            // Cleanup is a migration; projection/sync remain usable if it fails.
        }
    }

    // MARK: Presence

    private func projectPresence() {
        guard let room else { return }
        Task { @MainActor in
            let states = room.eph.getAllStates()
            var fresh: [String: Int64] = [:]
            for (key, value) in states where key.hasPrefix("presence/") {
                if let ms = value.i64Value {
                    fresh[String(key.dropFirst("presence/".count))] = ms
                }
            }
            presence = fresh
        }
    }

    func deviceOnline(_ deviceId: String) -> Bool {
        guard let ms = presence[deviceId] else { return false }
        return nowMs() - ms < presenceFreshMs
    }

    // MARK: Projection (doc → rows)

    private func project() {
        let value = doc.getDeepValue()
        guard let root = value.mapValue else { return }

        devices = (root["devices"]?.mapValue ?? [:]).compactMap { _, v in
            guard let m = v.mapValue, let id = m["id"]?.stringValue else { return nil }
            return DeviceRow(id: id,
                            name: m["name"]?.stringValue ?? id,
                            platform: m["platform"]?.stringValue ?? "",
                            lastSeenAt: m["lastSeenAt"]?.i64Value,
                            createdAt: m["createdAt"]?.i64Value)
        }.sorted { $0.name < $1.name }

        spaces = (root["spaces"]?.mapValue ?? [:]).compactMap { _, v in
            guard let m = v.mapValue, let id = m["id"]?.stringValue,
                  let deviceId = m["deviceId"]?.stringValue,
                  let path = m["path"]?.stringValue else { return nil }
            return Space(id: id, deviceId: deviceId, path: path,
                         name: m["name"]?.stringValue,
                         gitDetected: m["gitDetected"]?.boolValue ?? false,
                         gitCheckedAt: m["gitCheckedAt"]?.i64Value,
                         checkoutId: m["checkoutId"]?.stringValue,
                         createdAt: m["createdAt"]?.i64Value ?? 0)
        }.sorted { ($0.createdAt, $0.id) < ($1.createdAt, $1.id) }  // creation order, id tiebreak

        chats = (root["chats"]?.mapValue ?? [:]).compactMap { _, v in
            guard let m = v.mapValue, let id = m["id"]?.stringValue,
                  let deviceId = m["deviceId"]?.stringValue else { return nil }
            var chatConfig: ChatConfig?
            if let c = m["config"]?.mapValue {
                chatConfig = ChatConfig(harness: c["harness"]?.stringValue ?? "claude-code",
                                        model: c["model"]?.stringValue,
                                        reasoning: c["reasoning"]?.stringValue,
                                        sandbox: c["sandbox"]?.stringValue)
            }
            return Chat(id: id, deviceId: deviceId,
                        title: m["title"]?.stringValue,
                        archived: m["archived"]?.boolValue ?? false,
                        cwd: m["cwd"]?.stringValue,
                        branch: m["branch"]?.stringValue,
                        checkoutId: m["checkoutId"]?.stringValue,
                        config: chatConfig,
                        lastMessagePreview: m["lastMessagePreview"]?.stringValue,
                        lastMessageAt: m["lastMessageAt"]?.i64Value,
                        createdAt: m["createdAt"]?.i64Value ?? 0,
                        harnessSessionId: m["harnessSessionId"]?.stringValue,
                        harnessSessionCwd: m["harnessSessionCwd"]?.stringValue,
                        spaceId: m["spaceId"]?.stringValue,
                        lastSeenAt: m["lastSeenAt"]?.i64Value)
        }
        sessionRefs = (root["sessionRefs"]?.mapValue ?? [:]).compactMap { _, value in
            guard let row = value.mapValue,
                  row["userId"]?.stringValue == config.userId,
                  let rawChatId = row["chatId"]?.stringValue,
                  let uuid = UUID(uuidString: rawChatId),
                  let addedAt = row["addedAt"]?.i64Value else { return nil }
            let environment: SessionEnvironment? = row["environment"].flatMap { value in
                guard let data = try? JSONSerialization.data(withJSONObject: value.jsonObject) else {
                    return nil
                }
                return try? JSONDecoder().decode(SessionEnvironment.self, from: data)
            }
            return SessionRef(chatId: uuid.uuidString.lowercased(), addedAt: addedAt,
                              environment: environment)
        }.sorted {
            ($0.addedAt, $0.chatId) > ($1.addedAt, $1.chatId)
        }


        var rows: [String: SessionRow] = [:]
        for (_, v) in root["sessions"]?.mapValue ?? [:] {
            guard let m = v.mapValue, let chatId = m["chatId"]?.stringValue,
                  let deviceId = m["deviceId"]?.stringValue,
                  let statusStr = m["status"]?.stringValue,
                  let status = SessionStatus(rawValue: statusStr) else { continue }
            rows[chatId] = SessionRow(chatId: chatId, deviceId: deviceId, status: status,
                                      startedAt: m["startedAt"]?.i64Value,
                                      updatedAt: m["updatedAt"]?.i64Value ?? 0)
        }
        sessions = rows
    }

    // MARK: Derived views

    /// state.rs `overview_chats`: every non-archived chat of a live space,
    /// attention-sorted. A chat row always wins over a membership ref — the
    /// row carries the context (status, harness, branch) a bare ref lacks.
    var overviewChats: [Chat] {
        let liveSpaceIds = Set(spaces.map(\.id))
        let live = chats.filter {
            !$0.archived && $0.spaceId.map(liveSpaceIds.contains) == true
        }
        return sortActive(live)
    }
    /// Imported memberships with no workspace chat row — sessions genuinely
    /// foreign to this workspace. Row-backed refs are served by `overviewChats`.
    var sharedSessionRefs: [SessionRef] {
        let rowIds = Set(chats.map(\.id))
        return sessionRefs.filter { !rowIds.contains($0.chatId) }
    }


    /// A space's owned sessions, in the sidebar's Sessions order (recency).
    ///
    /// NOT desktop's `chats_in_space`, which is creation order because there
    /// the rows are TABS and activity must never reorder tabs. The phone has
    /// no tabs — a space opens into the same list, with the same rows, as the
    /// Sessions section — so it follows that list's ordering instead.
    func chats(in spaceId: String) -> [Chat] {
        sortActive(chats.filter { !$0.archived && $0.spaceId == spaceId })
    }

    func indicator(for chat: Chat) -> ChatIndicator {
        chatIndicator(chat: chat, live: effectiveStatus(sessions[chat.id], now: nowMs()))
    }

    /// Aggregate most-urgent member status for a space's leading dot.
    func spaceIndicator(_ spaceId: String) -> ChatIndicator? {
        let members = chats(in: spaceId).map { indicator(for: $0) }
        return members.min(by: { $0.rawValue < $1.rawValue })
    }

    // MARK: Device relay (folder browsing / direct host RPCs)

    @ObservationIgnored private var relayClients: [String: DeviceRelayClient] = [:]

    private func relay(for deviceId: String) -> DeviceRelayClient {
        if let existing = relayClients[deviceId] { return existing }
        let client = DeviceRelayClient(deviceId: deviceId, config: config)
        relayClients[deviceId] = client
        return client
    }

    /// The last relay failure, for surfacing in UI/diagnostics.
    private(set) var lastRelayError: String?

    /// ListFolders on the target device (engine caps at 500 entries, hides
    /// dotfiles, stamps isRepo). nil path = the device's home directory.
    func listFolders(deviceId: String, path: String?) async -> FolderListing? {
        do {
            return try await listFoldersDetailed(deviceId: deviceId, path: path)
        } catch {
            lastRelayError = error.localizedDescription
            return nil
        }
    }

    func listFoldersDetailed(deviceId: String, path: String?) async throws -> FolderListing {
        var params: [String: Any] = [:]
        if let path { params["path"] = path }
        return try await relay(for: deviceId).call(method: "ListFolders", params: params)
    }

    /// ListRefs on the target device — branches with current/worktree markers
    /// (default branch first, per the engine's ordering).
    func listRefs(deviceId: String, repoPath: String) async -> [RepoRef]? {
        try? await relay(for: deviceId).call(method: "ListRefs", params: ["repoPath": repoPath])
    }

    /// ListModels — the target device's live harness catalog (the desktop
    /// discovers models from the CLI itself; static lists are only fallback).
    func listModels(deviceId: String, harness: String) async -> [ModelInfo]? {
        struct WireModel: Decodable {
            var id: String
            var label: String
            var description: String?
            var reasoningLevels: [String]?
        }
        let wire: [WireModel]? = try? await relay(for: deviceId)
            .call(method: "ListModels", params: ["harness": harness])
        return wire.map { models in
            models.map {
                ModelInfo(id: $0.id, label: $0.label, description: $0.description,
                          reasoningLevels: $0.reasoningLevels ?? [])
            }
        }
    }

    /// SwitchRef — `git checkout` in the given folder on the target device.
    /// Returns git's error message on failure (dirty tree, held ref, …).
    func switchRef(deviceId: String, repoPath: String, refName: String) async -> String? {
        struct Reply: Decodable { var branch: String? }
        do {
            let _: Reply = try await relay(for: deviceId)
                .call(method: "SwitchRef", params: ["repoPath": repoPath, "refName": refName])
            return nil
        } catch {
            return error.localizedDescription
        }
    }

    /// CreateWorktree — a fresh isolated worktree off the base ref; returns
    /// its path.
    func createWorktree(deviceId: String, repoPath: String, branch: String) async -> String? {
        struct Reply: Decodable { var path: String }
        let reply: Reply? = try? await relay(for: deviceId)
            .call(method: "CreateWorktree", params: ["repoPath": repoPath, "branch": branch])
        return reply?.path
    }


    func forkSession(source: Chat) async throws -> String {
        struct Reply: Decodable { var chatId: String }
        let reply: Reply = try await relay(for: source.deviceId).call(
            method: "ForkSession", params: ["sourceChatId": source.id]
        )
        return reply.chatId
    }

    /// Create, attach, and start a Scaffold-hosted OMP session through the
    /// selected desktop controller. The desktop remains the trusted control
    /// plane client; iOS never receives sandbox bootstrap credentials.
    func launchScaffoldSession(space: Space, prompt: String,
                               launch: ScaffoldLaunchConfig) async throws -> (String, ScaffoldControlRoute) {
        let chatId = UUID().uuidString.lowercased()
        let requestedScope: [String: Any] = [
            "projectId": config.projectScope,
            "deploymentId": config.projectScope,
            "sessionId": chatId,
        ]
        let agentRoute: [String: Any] = [
            "provider": launch.provider,
            "model": launch.providerModel,
            "fallback": "disabled",
            "routingMode": "automatic",
        ]
        let create: ScaffoldEnvironmentControlResult = try await relay(for: space.deviceId).call(
            method: "ControlScaffoldEnvironment",
            params: [
                "operation": "create",
                "scope": requestedScope,
                "source_ref": launch.sourceRef,
                "database_environment": launch.databaseEnvironment.rawValue,
                "agentRoute": agentRoute,
            ],
            timeoutNanoseconds: 30_000_000_000
        )
        guard create.environment.source.kind == "scaffold",
              let sandboxId = create.environment.source.sandboxId else {
            throw MobileSessionError.unavailable("Scaffold returned an invalid environment")
        }

        let scope = encodableDictionary(create.environment.scope)
        let attachment = try await attachScaffoldEnvironment(
            controllerDeviceId: space.deviceId, sandboxId: sandboxId, scope: scope
        )
        guard let ownerDeviceId = attachment.attachedDeviceId,
              let projection = attachment.roomProjection,
              let grant = attachment.controlGrant,
              grant.capabilities.contains("session.chat") else {
            throw MobileSessionError.unavailable("Scaffold returned no chat authority")
        }

        try await waitForScaffoldReadiness(
            controllerDeviceId: space.deviceId, sandboxId: sandboxId, scope: scope
        )

        let chatConfig = ChatConfig(harness: "omp", model: launch.persistedModel,
                                    reasoning: launch.reasoning, sandbox: "workspace-write")
        struct OkReply: Decodable { var ok: Bool? }
        let _: OkReply = try await relay(for: space.deviceId).call(
            method: "Mutate",
            params: [
                "op": "createChat",
                "chatId": chatId,
                "spaceId": space.id,
                "cwd": ".",
                "branch": launch.sourceRef,
                "config": encodableDictionary(chatConfig),
            ]
        )
        try putChat(chatId: chatId, space: space, config: chatConfig,
                    branch: launch.sourceRef, cwd: ".")
        _ = addSessionRef(chatId: chatId, environment: attachment.environment)

        let route = ScaffoldControlRoute(
            controllerDeviceId: space.deviceId,
            ownerDeviceId: ownerDeviceId,
            actorSubject: attachment.environment.ownerPrincipal,
            grantId: grant.id,
            projection: projection,
            environment: attachment.environment
        )
        let request = RunRequest(prompt: prompt, model: chatConfig.model,
                                 reasoning: chatConfig.reasoning, cwd: ".",
                                 sandbox: "workspace-write")
        try await queueScaffoldCommand(
            route: route, payload: .run(request: request,
                                       messageId: UUID().uuidString.lowercased())
        )
        return (chatId, route)
    }

    /// Ordinary commands must be admitted on their actual host. A different
    /// desktop's local trust record cannot authorize this host's ledger drain.
    func sendSessionCommand(chatId: String, payload: SessionCommandPayload) async throws {
        guard let hostDeviceId = chats.first(where: { $0.id == chatId })?.deviceId
            ?? sessions[chatId]?.deviceId, !hostDeviceId.isEmpty else {
            throw MobileSessionError.unavailable("This session has no known desktop host")
        }
        var command: [String: Any] = ["kind": payload.kind]
        switch payload {
        case .run(let request, let messageId):
            command["request"] = encodableDictionary(request)
            command["messageId"] = messageId
        case .steer(let prompt, let messageId):
            command["prompt"] = prompt
            if let messageId { command["messageId"] = messageId }
        case .interrupt:
            break
        case .respondInput(let requestId, let answers):
            command["requestId"] = requestId
            command["answers"] = answers.map(encodableDictionary)
        }
        struct Reply: Decodable { var commandId: String }
        let _: Reply = try await relay(for: hostDeviceId).call(
            method: "QueueCommand",
            params: [
                "chatId": chatId,
                "commandId": UUID().uuidString.lowercased(),
                "command": command,
            ]
        )
    }

    func sendScaffoldCommand(controllerDeviceId: String,
                             environment: SessionEnvironment,
                             payload: SessionCommandPayload) async throws {
        guard environment.source.kind == "scaffold",
              let sandboxId = environment.source.sandboxId else {
            throw MobileSessionError.unavailable("This session has no Scaffold route")
        }
        let scope = encodableDictionary(environment.scope)
        let attachment = try await attachScaffoldEnvironment(
            controllerDeviceId: controllerDeviceId, sandboxId: sandboxId, scope: scope
        )
        guard let ownerDeviceId = attachment.attachedDeviceId,
              let projection = attachment.roomProjection,
              let grant = attachment.controlGrant,
              grant.capabilities.contains("session.chat") else {
            throw MobileSessionError.unavailable("Scaffold returned no chat authority")
        }
        let route = ScaffoldControlRoute(
            controllerDeviceId: controllerDeviceId,
            ownerDeviceId: ownerDeviceId,
            actorSubject: attachment.environment.ownerPrincipal,
            grantId: grant.id,
            projection: projection,
            environment: attachment.environment
        )
        _ = addSessionRef(chatId: projection.sessionId, environment: attachment.environment)
        try await queueScaffoldCommand(route: route, payload: payload)
    }

    private func attachScaffoldEnvironment(controllerDeviceId: String, sandboxId: String,
                                           scope: [String: Any]) async throws -> ScaffoldEnvironmentControlResult {
        let deadline = Date().addingTimeInterval(90)
        var lastError: Error?
        repeat {
            do {
                return try await relay(for: controllerDeviceId).call(
                    method: "ControlScaffoldEnvironment",
                    params: ["operation": "attach", "sandbox_id": sandboxId, "scope": scope],
                    timeoutNanoseconds: 30_000_000_000
                )
            } catch {
                lastError = error
                try? await Task.sleep(nanoseconds: 500_000_000)
            }
        } while Date() < deadline
        throw lastError ?? RelayError.timeout
    }

    private func waitForScaffoldReadiness(controllerDeviceId: String, sandboxId: String,
                                          scope: [String: Any]) async throws {
        let deadline = Date().addingTimeInterval(120)
        var lastLifecycle = "unknown"
        repeat {
            let inspected: ScaffoldEnvironmentControlResult = try await relay(for: controllerDeviceId).call(
                method: "ControlScaffoldEnvironment",
                params: ["operation": "inspect", "sandbox_id": sandboxId, "scope": scope],
                timeoutNanoseconds: 30_000_000_000
            )
            lastLifecycle = inspected.environment.source.lifecycle ?? "unknown"
            if lastLifecycle == "ready" || lastLifecycle == "agent_running" { return }
            if lastLifecycle == "failed" || lastLifecycle == "stopped" {
                throw MobileSessionError.unavailable("Scaffold session became \(lastLifecycle)")
            }
            try? await Task.sleep(nanoseconds: 500_000_000)
        } while Date() < deadline
        throw MobileSessionError.unavailable("Scaffold session remained \(lastLifecycle)")
    }

    private func queueScaffoldCommand(route: ScaffoldControlRoute,
                                      payload: SessionCommandPayload) async throws {
        let actionPayload: [String: Any]
        switch payload {
        case .run(let request, let messageId):
            actionPayload = [
                "action": "start",
                "request": encodableDictionary(request),
                "message_id": messageId,
            ]
        case .steer(let prompt, let messageId):
            var action: [String: Any] = ["action": "steer", "prompt": prompt]
            if let messageId { action["message_id"] = messageId }
            actionPayload = action
        case .interrupt:
            actionPayload = ["action": "stop"]
        case .respondInput(let requestId, let answers):
            actionPayload = [
                "action": "respondInput",
                "request_id": requestId,
                "answers": answers.map(encodableDictionary),
            ]
        }
        let command: [String: Any] = [
            "kind": "control",
            "sessionId": route.projection.sessionId,
            "ownerDeviceId": route.ownerDeviceId,
            "actorDeviceId": route.controllerDeviceId,
            "actorSubject": route.actorSubject,
            "grantId": route.grantId,
            "source": "scaffold",
            "action": actionPayload,
        ]
        struct Reply: Decodable { var commandId: String }
        let _: Reply = try await relay(for: route.controllerDeviceId).call(
            method: "QueueCommand",
            params: [
                "chatId": route.projection.sessionId,
                "commandId": UUID().uuidString.lowercased(),
                "command": command,
            ],
            timeoutNanoseconds: 30_000_000_000
        )
    }

    private func encodableDictionary<T: Encodable>(_ value: T) -> [String: Any] {
        guard let data = try? JSONEncoder().encode(value),
              let object = try? JSONSerialization.jsonObject(with: data),
              let dictionary = object as? [String: Any] else { return [:] }
        return dictionary
    }

    /// Retarget a session onto another checkout (the desktop's
    /// setChatCwd/setChatBranch mutates — LWW row writes here).
    func setChatCheckout(chatId: String, cwd: String, branch: String) {
        updateChat(chatId) { row in
            try row.insert(key: "cwd", v: cwd)
            try row.insert(key: "branch", v: branch)
        }
    }

    // MARK: Writes (viewer-device discipline)
    private func sessionRefKey(chatId: String) -> String {
        "\(config.userId.utf8.count):\(config.userId):\(chatId)"
    }

    @discardableResult
    func addSessionRef(chatId rawChatId: String, environment: SessionEnvironment? = nil) -> SessionRef? {
        guard let uuid = UUID(uuidString: rawChatId.trimmingCharacters(in: .whitespacesAndNewlines))
        else { return nil }
        let chatId = uuid.uuidString.lowercased()
        let map = doc.getMap(id: "sessionRefs")
        do {
            let row = try map.getOrCreateContainer(
                key: sessionRefKey(chatId: chatId), child: LoroMap()
            )
            try row.insert(key: "userId", v: config.userId)
            try row.insert(key: "chatId", v: chatId)
            let addedAt = row.get(key: "addedAt")?.asValue()?.i64Value ?? nowMs()
            try row.insert(key: "addedAt", v: addedAt)
            if let environment, let value = LoroValue.fromEncodable(environment) {
                try row.insert(key: "environment", v: value)
            }
            doc.commit()
            project()
            return SessionRef(chatId: chatId, addedAt: addedAt, environment: environment)
        } catch {
            return nil
        }
    }

    /// Remove only this workspace's membership; the `s2/{chatId}` room remains.
    func removeSessionRef(chatId: String) {
        let map = doc.getMap(id: "sessionRefs")
        do {
            try map.delete(key: sessionRefKey(chatId: chatId))
            doc.commit()
            project()
        } catch {}
    }


    /// Create through the actual host so its verified principal membership is
    /// installed before the first QueueCommand can reach command draining.
    @discardableResult
    func createChat(space: Space, config chatConfig: ChatConfig,
                    branch: String? = nil, cwd: String? = nil) async throws -> String {
        guard !space.deviceId.isEmpty else {
            throw MobileSessionError.unavailable("This space has no desktop host")
        }
        let chatId = UUID().uuidString.lowercased()
        var params: [String: Any] = [
            "op": "createChat",
            "chatId": chatId,
            "spaceId": space.id,
            "config": encodableDictionary(chatConfig),
        ]
        if let branch { params["branch"] = branch }
        if let cwd { params["cwd"] = cwd }
        struct Reply: Decodable { var ok: Bool }
        let reply: Reply = try await relay(for: space.deviceId).call(method: "Mutate", params: params)
        guard reply.ok else {
            throw MobileSessionError.unavailable("The desktop did not create this session")
        }
        try putChat(chatId: chatId, space: space, config: chatConfig,
                    branch: branch, cwd: cwd ?? space.path)
        return chatId
    }

    private func putChat(chatId: String, space: Space, config chatConfig: ChatConfig,
                         branch: String?, cwd: String) throws {
        let map = doc.getMap(id: "chats")
        let row = try map.getOrCreateContainer(key: chatId, child: LoroMap())
        try row.insert(key: "id", v: chatId)
        try row.insert(key: "deviceId", v: space.deviceId)
        try row.insert(key: "archived", v: false)
        try row.insert(key: "cwd", v: cwd)
        try row.insert(key: "spaceId", v: space.id)
        let createdAt = row.get(key: "createdAt")?.asValue()?.i64Value ?? nowMs()
        try row.insert(key: "createdAt", v: createdAt)
        if let branch { try row.insert(key: "branch", v: branch) }
        if let value = LoroValue.fromEncodable(chatConfig) {
            try row.insert(key: "config", v: value)
        }
        doc.commit()
        project()
    }

    /// Create a space. Preferred path: `Mutate {op:createSpace}` straight to
    /// the owning host over its relay (it applies the row to its own workspace
    /// doc, functionally identical to the desktop's local mutate + sync).
    /// Fallback when the host is unreachable: LWW row write into our mirror —
    /// creates are legal from any device; the owner stamps git on arrival.
    @discardableResult
    func createSpace(deviceId: String, path: String, gitDetected: Bool = false) async -> String {
        // Dedup on (device, path) like the desktop palette.
        if let existing = spaces.first(where: { $0.deviceId == deviceId && $0.path == path }) {
            return existing.id
        }
        let spaceId = UUID().uuidString.lowercased()
        struct OkReply: Decodable { var ok: Bool? }
        let params: [String: Any] = [
            "op": "createSpace",
            "spaceId": spaceId,
            "deviceId": deviceId,
            "path": path,
            "gitDetected": gitDetected,
        ]
        let viaHost: OkReply? = try? await relay(for: deviceId).call(method: "Mutate", params: params)
        if viaHost == nil {
            let map = doc.getMap(id: "spaces")
            do {
                let row = try map.getOrCreateContainer(key: spaceId, child: LoroMap())
                try row.insert(key: "id", v: spaceId)
                try row.insert(key: "deviceId", v: deviceId)
                try row.insert(key: "path", v: path)
                try row.insert(key: "gitDetected", v: gitDetected)
                try row.insert(key: "createdAt", v: nowMs())
                doc.commit()
            } catch {}
        }
        project()
        return spaceId
    }

    func setArchived(chatId: String, archived: Bool) {
        updateChat(chatId) { row in
            try row.insert(key: "archived", v: archived)
        }
    }

    func markSeen(chatId: String) {
        updateChat(chatId) { row in
            try row.insert(key: "lastSeenAt", v: nowMs())
        }
    }

    func rename(chatId: String, title: String) {
        updateChat(chatId) { row in
            try row.insert(key: "title", v: title)
        }
    }

    /// Chat config is an LWW map set on the chat row; the host reads it when
    /// dispatching the next run.
    func setChatConfig(chatId: String, config chatConfig: ChatConfig) {
        updateChat(chatId) { row in
            if let value = LoroValue.fromEncodable(chatConfig) {
                try row.insert(key: "config", v: value)
            }
        }
    }

    private func updateChat(_ chatId: String, _ mutate: (LoroMap) throws -> Void) {
        let map = doc.getMap(id: "chats")
        guard let row = map.get(key: chatId)?.asLoroMap() else { return }
        do {
            try mutate(row)
            doc.commit()
            project()
        } catch {}
    }
}
