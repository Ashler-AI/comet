// Workspace doc mirror. The edge binds this app to its verified project room;
// rows are globally shared within that project while `sessionRefs` are scoped
// to the authenticated principal. iOS is a viewport, not an engine device, so
// it deliberately owns neither a device row nor a presence heartbeat.

import Foundation
import Loro
import Observation

/// A first send owns this receipt until durable admission, independently of
/// navigation and cancellation of the task that prepared its attachments.
@MainActor
final class ScaffoldPreparationReceipt {
    let generation: String
    private(set) var admitted = false
    private var reported = false
    private let report: (String) async -> Void

    init(generation: String, report: @escaping (String) async -> Void) {
        self.generation = generation
        self.report = report
    }

    func markAdmitted() { admitted = true }

    func reportFailure() {
        guard !admitted, !reported else { return }
        reported = true
        // An unstructured task does not inherit the cancelled sender's state.
        let report = report
        let generation = generation
        Task { await report(generation) }
    }
}

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

    private(set) var doc = LoroDoc()
    private var room: RoomClient?
    private var subscriptions: [Subscription] = []
    @ObservationIgnored private var roomEpoch: UInt64 = 0
    @ObservationIgnored private var roomReadyGeneration: UInt64?
    private let config: AppConfig
    @ObservationIgnored var onProjection: (() -> Void)?

    init(config: AppConfig) {
        self.config = config
    }

    @ObservationIgnored private var saver: DocSaver?

    func start() {
        guard room == nil else { return }
        roomEpoch &+= 1
        let epoch = roomEpoch
        let roomId = "ws4/\(config.projectScope)"
        let cacheId = config.documentCacheId(roomId: roomId)
        // Hydrate locally before joining; materialize the sidebar off-main.
        _ = DocDisk.load(into: doc, id: cacheId)
        saver = DocSaver(docId: cacheId, doc: doc)
        let client = RoomClient(roomId: roomId, doc: doc) { [config] in
            await config.workspaceSocketURL()
        } events: { [weak self] event in
            Task { @MainActor [weak self] in
                guard let self, self.roomEpoch == epoch else { return }
                self.handle(event)
            }
        } adoptSnapshot: { [weak self] previous, replacement in
            guard let self, self.roomEpoch == epoch else { return false }
            return self.adoptSnapshot(previous: previous, replacement: replacement)
        }
        room = client

        subscribeLocalUpdates(client: client)

        Task { await client.start() }
        scheduleProjection()
    }

    private func subscribeLocalUpdates(client: RoomClient) {
        // Local commits → room. The subscription fires synchronously inside
        // commit; hop to the actor to send.
        subscriptions.append(doc.subscribeLocalUpdate { [weak client, weak self] update in
            guard let client else { return }
            let bytes = [UInt8](update)
            Task { await client.sendLocalUpdate(bytes) }
            Task { @MainActor [weak self] in self?.saver?.poke() }
        })
    }

    private func adoptSnapshot(previous: LoroDoc, replacement: LoroDoc) -> Bool {
        guard doc === previous, let room,
              DocDisk.preserveLocalOperations(from: previous, in: replacement) else { return false }
        // No suspension between the final local-op merge and binding swap.
        // Keep projections, presence, relays, and all non-doc store state alive.
        subscriptions.removeAll()
        doc = replacement
        subscribeLocalUpdates(client: room)
        saver?.replaceDocument(with: replacement)
        scheduleProjection()
        return true
    }

    /// Backgrounding hook: persist immediately.
    func flushToDisk() {
        saver?.flush()
    }

    func stop() {
        roomEpoch &+= 1
        projectionGeneration &+= 1
        projectionTask?.cancel()
        projectionTask = nil
        subscriptions.removeAll()
        saver?.flush()
        if let room {
            Task { await room.stop() }
        }
        room = nil
        roomReadyGeneration = nil
        connected = false
    }

    private func handle(_ event: RoomEvent) {
        switch event {
        case .connected:
            roomReadyGeneration = projectionGeneration &+ 1
            scheduleProjection()
        case .disconnected:
            roomReadyGeneration = nil
            connected = false
        case .remoteUpdate:
            scheduleProjection()
            saver?.poke()
        case .ephemeralUpdate:
            projectPresence()
        }
    }

    /// Older iOS builds registered themselves as engine devices. Mobile is a
    /// controller only: remove those synced rows so desktop device pickers do
    /// not retain simulator/phone model names forever.
    private func purgeLegacyMobileDevices(_ staleIds: [String]) {
        guard !staleIds.isEmpty else { return }
        let map = doc.getMap(id: "devices")
        do {
            for id in staleIds {
                // Recheck only candidate rows: a newer remote edit can land
                // after the background read without its event arriving yet.
                guard let row = map.get(key: id) else { continue }
                let platform = row.asValue()?.mapValue?["platform"]?.stringValue
                    ?? row.asLoroMap()?.get(key: "platform")?.asValue()?.stringValue
                guard platform == "ios" else { continue }
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

    /// One immutable projection is built off-main, including the lists and indexes
    /// consumed by rows. Status-only updates never rebuild lists on the UI actor.
    struct Projection {
        var devices: [DeviceRow]
        var spaces: [Space]
        var chats: [Chat]
        var sessions: [String: SessionRow]
        var sessionRefs: [SessionRef]
        var legacyMobileIds: [String]
        var lists: WorkspaceLists

        init(devices: [DeviceRow], spaces: [Space], chats: [Chat],
             sessions: [String: SessionRow], sessionRefs: [SessionRef], legacyMobileIds: [String],
             lists: WorkspaceLists? = nil) {
            self.devices = devices
            self.spaces = spaces
            self.chats = chats
            self.sessions = sessions
            self.sessionRefs = sessionRefs
            self.legacyMobileIds = legacyMobileIds
            self.lists = lists ?? WorkspaceLists(devices: devices, spaces: spaces, chats: chats, refs: sessionRefs)
        }
    }

    @ObservationIgnored private var projectionTask: Task<Void, Never>?
    @ObservationIgnored private var projectionGeneration: UInt64 = 0
    @ObservationIgnored private var localProjectionGeneration: UInt64 = 0
    @ObservationIgnored private var previousProjection: Projection?
    #if DEBUG
    // Regression barrier: inject an event after a real decode, before its result
    // reaches the list. No timing assumptions or network/user data are involved.
    @ObservationIgnored private var projectionReadDidFinish: (() -> Void)?
    #endif

    /// Local writes retain their synchronous read-after-write contract. Invalidating
    /// the local generation prevents an older background read undoing an optimistic edit.
    private func project() {
        projectionGeneration &+= 1
        localProjectionGeneration &+= 1
        if let decoded = Self.decodeProjection(from: doc, userId: config.userId, previous: previousProjection) {
            applyProjection(decoded)
        }
    }

    /// A burst has at most one conversion in flight and one trailing conversion.
    /// Remote updates request a trailing read, not rejection of the current read:
    /// otherwise continuous room traffic can starve every list/status publication.
    private func scheduleProjection() {
        projectionGeneration &+= 1
        guard projectionTask == nil else { return }
        projectionTask = Task { @MainActor [weak self] in
            while let self, !Task.isCancelled {
                let generation = self.projectionGeneration
                let localGeneration = self.localProjectionGeneration
                let epoch = self.roomEpoch
                let document = self.doc
                let userId = self.config.userId
                let previous = self.previousProjection
                let decoded = await Task.detached(priority: .userInitiated) {
                    Self.decodeProjection(from: document, userId: userId, previous: previous)
                }.value
                #if DEBUG
                self.projectionReadDidFinish?()
                #endif
                guard !Task.isCancelled else { return }
                if self.roomEpoch == epoch, self.doc === document,
                   self.localProjectionGeneration == localGeneration, let decoded {
                    self.purgeLegacyMobileDevices(decoded.legacyMobileIds)
                    self.applyProjection(decoded)
                    // Do not report initial/reconnect readiness while the UI
                    // still holds an older cached projection of this replica.
                    if let readyGeneration = self.roomReadyGeneration, generation >= readyGeneration {
                        self.connected = true
                    }
                }
                if self.projectionGeneration == generation {
                    self.projectionTask = nil
                    return
                }
            }
        }
    }

    #if DEBUG
    static func runLiveListProjectionRegression() async -> Bool {
        let config = AppConfig(edgeURL: URL(string: "http://127.0.0.1:1")!, mode: .dev,
                               userId: "list-regression", projectScope: "list-regression",
                               deviceId: "viewer", deviceName: "Crew regression")
        let store = WorkspaceStore(config: config)
        defer { store.stop() }
        do {
            let row = try store.doc.getMap(id: "chats").getOrCreateContainer(key: "row", child: LoroMap())
            try row.insert(key: "id", v: "row")
            try row.insert(key: "deviceId", v: "host")
            try row.insert(key: "title", v: "Remote 0")
            let ref = try store.doc.getMap(id: "sessionRefs").getOrCreateContainer(key: "membership", child: LoroMap())
            try ref.insert(key: "chatId", v: "row")
            try ref.insert(key: "userId", v: config.userId)
            try ref.insert(key: "addedAt", v: Int64(1))
            store.doc.commit()
            var reads = 0
            var titles: [String] = []
            var mutationFailed = false
            store.onProjection = { titles.append(store.overviewChats.first?.title ?? "missing") }
            defer { store.onProjection = nil; store.projectionReadDidFinish = nil }
            store.projectionReadDidFinish = {
                reads += 1
                guard reads <= 5 else { return }
                do {
                    try row.insert(key: "title", v: "Remote \(reads)")
                    store.doc.commit()
                    store.handle(.remoteUpdate)
                } catch { mutationFailed = true }
            }
            store.handle(.connected)
            guard !store.connected else { return false }
            guard await E2ERunner.poll(timeout: 5, label: "live list projection burst", {
                store.projectionTask == nil ? true : nil
            }) != nil else { return false }
            guard !mutationFailed, store.connected,
                  titles == (0...5).map({ "Remote \($0)" }) else { return false }

            // A synchronous optimistic rename must still fence an older read.
            titles.removeAll()
            store.projectionReadDidFinish = {
                store.projectionReadDidFinish = nil
                store.rename(chatId: "row", title: "Local edit")
            }
            store.scheduleProjection()
            guard await E2ERunner.poll(timeout: 5, label: "live list local-write fence", {
                store.projectionTask == nil ? true : nil
            }) != nil else { return false }
            return !titles.isEmpty && titles.allSatisfy { $0 == "Local edit" }
                && store.overviewChats.first?.title == "Local edit"
        } catch { return false }
    }
    #endif

    nonisolated static func decodeProjection(from doc: LoroDoc, userId: String,
                                             previous: Projection? = nil) -> Projection? {
        let value = doc.getDeepValue()
        guard let root = value.mapValue else { return nil }

        let devices: [DeviceRow] = (root["devices"]?.mapValue ?? [:]).compactMap { _, v in
            guard let m = v.mapValue, let id = m["id"]?.stringValue,
                  m["platform"]?.stringValue != "ios" else { return nil }
            return DeviceRow(id: id,
                            name: m["name"]?.stringValue ?? id,
                            platform: m["platform"]?.stringValue ?? "",
                            lastSeenAt: m["lastSeenAt"]?.i64Value,
                            createdAt: m["createdAt"]?.i64Value)
        }.sorted { ($0.name, $0.id) < ($1.name, $1.id) }

        let spaces: [Space] = (root["spaces"]?.mapValue ?? [:]).compactMap { _, v in
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

        let projectChats: [Chat] = (root["chats"]?.mapValue ?? [:]).compactMap { _, v in
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
        let sessionRefs: [SessionRef] = (root["sessionRefs"]?.mapValue ?? [:]).compactMap { _, value in
            guard let row = value.mapValue,
                  row["userId"]?.stringValue == userId,
                  let chatId = row["chatId"]?.stringValue,
                  let addedAt = row["addedAt"]?.i64Value else { return nil }
            let environment: SessionEnvironment? = row["environment"].flatMap { value in
                guard let data = try? JSONSerialization.data(withJSONObject: value.jsonObject) else {
                    return nil
                }
                return try? JSONDecoder().decode(SessionEnvironment.self, from: data)
            }
            let startup: SessionStartup? = row["startup"].flatMap { value in
                guard let data = try? JSONSerialization.data(withJSONObject: value.jsonObject) else { return nil }
                return try? JSONDecoder().decode(SessionStartup.self, from: data)
            }
            return SessionRef(chatId: chatId, addedAt: addedAt,
                              environment: environment, startup: startup)
        }.sorted {
            if $0.addedAt != $1.addedAt { return $0.addedAt > $1.addedAt }
            return $0.chatId < $1.chatId
        }
        // The project room is shared, but sidebar membership is per principal,
        // exactly like WorkspaceHost::retain_visible_sessions. Never mutate
        // other project rows to implement this presentation boundary.
        let memberIds = Set(sessionRefs.map(\.chatId))
        let chats = projectChats.filter { memberIds.contains($0.id) }


        var rows: [String: SessionRow] = [:]
        for (_, v) in root["sessions"]?.mapValue ?? [:] {
            guard let m = v.mapValue, let chatId = m["chatId"]?.stringValue,
                  let deviceId = m["deviceId"]?.stringValue,
                  let statusStr = m["status"]?.stringValue,
                  memberIds.contains(chatId),
                  let status = SessionStatus(rawValue: statusStr) else { continue }
            rows[chatId] = SessionRow(chatId: chatId, deviceId: deviceId, status: status,
                                      startedAt: m["startedAt"]?.i64Value,
                                      updatedAt: m["updatedAt"]?.i64Value ?? 0)
        }
        let orderedChats = chats.sorted { $0.id < $1.id }
        let reusableLists = previous.flatMap { previous in
            previous.devices == devices && previous.spaces == spaces && previous.chats == orderedChats
                && previous.sessionRefs == sessionRefs ? previous.lists : nil
        }
        return Projection(devices: devices, spaces: spaces, chats: orderedChats,
                          sessions: rows, sessionRefs: sessionRefs,
                          legacyMobileIds: (root["devices"]?.mapValue ?? [:]).compactMap { id, value in
                              value.mapValue?["platform"]?.stringValue == "ios" ? id : nil
                          }, lists: reusableLists)
    }

    func applyProjection(_ decoded: Projection) {
        previousProjection = decoded
        if devices != decoded.devices { devices = decoded.devices }
        if spaces != decoded.spaces { spaces = decoded.spaces }
        if chats != decoded.chats { chats = decoded.chats }
        if sessions != decoded.sessions { sessions = decoded.sessions }
        if sessionRefs != decoded.sessionRefs { sessionRefs = decoded.sessionRefs }
        let next = decoded.lists
        if overviewChats != next.overviewChats { overviewChats = next.overviewChats }
        if settledChats != next.settledChats { settledChats = next.settledChats }
        if sharedSessionRefs != next.sharedSessionRefs { sharedSessionRefs = next.sharedSessionRefs }
        if occupiedSpaces != next.occupiedSpaces { occupiedSpaces = next.occupiedSpaces }
        if activeBySpace != next.activeBySpace { activeBySpace = next.activeBySpace }
        if archivedBySpace != next.archivedBySpace { archivedBySpace = next.archivedBySpace }
        if chatsById != next.chatsById { chatsById = next.chatsById }
        if spacesById != next.spacesById { spacesById = next.spacesById }
        if devicesById != next.devicesById { devicesById = next.devicesById }
        if refsById != next.refsById { refsById = next.refsById }
        lists = next
        // Notifications must see the complete new projection, even when no list
        // changed (session freshness/status can be the only updated field).
        onProjection?()
    }

    // MARK: Derived views

    @ObservationIgnored private(set) var lists = WorkspaceLists()
    private(set) var overviewChats: [Chat] = []
    private(set) var settledChats: [Chat] = []
    private(set) var sharedSessionRefs: [SessionRef] = []
    private(set) var occupiedSpaces: [Space] = []
    private var activeBySpace: [String: [Chat]] = [:]
    private var archivedBySpace: [String: [Chat]] = [:]
    private var chatsById: [String: Chat] = [:]
    private var spacesById: [String: Space] = [:]
    private var devicesById: [String: DeviceRow] = [:]
    private var refsById: [String: SessionRef] = [:]

    func chats(in spaceId: String) -> [Chat] { activeBySpace[spaceId] ?? [] }
    func settledChats(in spaceId: String) -> [Chat] { archivedBySpace[spaceId] ?? [] }
    func chat(id: String) -> Chat? { chatsById[id] }
    func space(id: String) -> Space? { spacesById[id] }
    func device(id: String) -> DeviceRow? { devicesById[id] }
    func sessionRef(id: String) -> SessionRef? { refsById[id] }


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

    /// Catalogs are discovered on the host that owns the selected space.
    func listHarnesses(deviceId: String) async throws -> [HarnessInfo] {
        struct WireHarness: Decodable {
            var id: String
            var name: String
        }
        let wire: [WireHarness] = try await relay(for: deviceId)
            .call(method: "ListHarnesses", params: [:])
        return wire.map { HarnessInfo(id: $0.id, label: $0.name) }
    }

    func listModels(deviceId: String, harness: String) async throws -> [ModelInfo] {
        struct WireModel: Decodable {
            var id: String
            var label: String
            var description: String?
            var reasoningLevels: [String]?
        }
        let wire: [WireModel] = try await relay(for: deviceId)
            .call(method: "ListModels", params: ["harness": harness])
        return wire.map {
            ModelInfo(id: $0.id, label: $0.label, description: $0.description,
                      reasoningLevels: $0.reasoningLevels ?? [])
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

    /// Prepare a Scaffold route before uploading attachments or admitting the
    /// first run. A failed attach/upload retry retains the created environment.
    func prepareScaffoldSession(space: Space, chatId: String,
                                launch: ScaffoldLaunchConfig) async throws -> (ScaffoldControlRoute, ScaffoldPreparationReceipt) {
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
        try Task.checkCancellation()
        // Decode the receipt even when the rest of the response is malformed.
        struct PreparedReply: Decodable {
            var generation: String?
            var attachment: Result<ScaffoldEnvironmentControlResult, Error>
            enum CodingKeys: String, CodingKey { case preparationGeneration }
            init(from decoder: Decoder) throws {
                let fields = try decoder.container(keyedBy: CodingKeys.self)
                generation = try fields.decodeIfPresent(String.self, forKey: .preparationGeneration)
                attachment = Result { try ScaffoldEnvironmentControlResult(from: decoder) }
            }
        }
        let reply: PreparedReply = try await relay(for: space.deviceId).call(
            method: "PrepareScaffoldSession",
            params: [
                "scope": requestedScope,
                "name": NSNull(),
                "ompHandoff": NSNull(),
                "sourceRef": launch.sourceRef,
                "databaseEnvironment": launch.databaseEnvironment.rawValue,
                "agentRoute": agentRoute,
            ],
            timeoutNanoseconds: nil,
            preserveSuccessfulResponseOnCancellation: true
        )
        guard let generation = reply.generation, !generation.isEmpty else {
            throw MobileSessionError.unavailable("Scaffold returned no preparation generation")
        }
        let receipt = ScaffoldPreparationReceipt(generation: generation) { [self] generation in
            await reportScaffoldPreparationFailure(
                controllerDeviceId: space.deviceId, chatId: chatId, generation: generation
            )
        }
        var transferred = false
        defer { if !transferred { receipt.reportFailure() } }
        let attachment = try reply.attachment.get()
        try Task.checkCancellation()
        guard attachment.environment.source.kind == "scaffold",
              attachment.environment.source.sandboxId != nil,
              attachment.environment.scope.projectId == config.projectScope,
              attachment.environment.scope.deploymentId == config.projectScope,
              attachment.environment.scope.sessionId == chatId else {
            throw MobileSessionError.unavailable("Scaffold returned a different session identity")
        }
        guard let ownerDeviceId = attachment.attachedDeviceId,
              let projection = attachment.roomProjection,
              let grant = attachment.controlGrant,
              grant.capabilities.contains("session.chat") else {
            throw MobileSessionError.unavailable("Scaffold returned no chat authority")
        }


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
            environment: attachment.environment,
            preparationGeneration: generation
        )
        transferred = true
        return (route, receipt)
    }

    private func reportScaffoldPreparationFailure(controllerDeviceId: String,
                                                   chatId: String, generation: String) async {
        struct Reply: Decodable { var reported: Bool }
        let _: Reply? = try? await relay(for: controllerDeviceId).call(
            method: "ReportScaffoldPreparationFailure",
            params: ["chatId": chatId, "generation": generation]
        )
    }

    /// Ordinary commands must be admitted on their actual host. A different
    /// desktop's local trust record cannot authorize this host's ledger drain.
    func sendSessionCommand(chatId: String, payload: SessionCommandPayload) async throws {
        guard let hostDeviceId = chat(id: chatId)?.deviceId
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
                "commandId": payload.messageId ?? UUID().uuidString.lowercased(),
                "command": command,
            ]
        )
    }

    func sendScaffoldCommand(controllerDeviceId: String,
                             environment: SessionEnvironment,
                             payload: SessionCommandPayload,
                             preparationGeneration: String? = nil) async throws {
        let route = try await scaffoldRoute(controllerDeviceId: controllerDeviceId, environment: environment)
        try await queueScaffoldCommand(route: route, payload: payload,
                                       preparationGeneration: preparationGeneration)
    }

    func scaffoldRoute(controllerDeviceId: String, environment: SessionEnvironment) async throws -> ScaffoldControlRoute {
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
        return route
    }

    func uploadImages(_ images: [MobileImageAttachment], chatId: String, deviceId: String) async throws -> [String] {
        struct OkReply: Decodable { var ok: Bool }
        struct CommitReply: Decodable { var path: String }
        var paths: [String] = []
        for image in images {
            let uploadId = UUID().uuidString.lowercased()
            for (seq, offset) in stride(from: 0, to: image.bytes.count, by: 45_000).enumerated() {
                try Task.checkCancellation()
                let end = min(offset + 45_000, image.bytes.count)
                let reply: OkReply = try await relay(for: deviceId).call(
                    method: "UploadChunk",
                    params: ["uploadId": uploadId, "seq": seq, "sessionId": chatId,
                             "data": image.bytes[offset..<end].base64EncodedString()],
                    timeoutNanoseconds: 90_000_000_000
                )
                guard reply.ok else { throw MobileSessionError.unavailable("The host rejected an image chunk") }
            }
            try Task.checkCancellation()
            let reply: CommitReply = try await relay(for: deviceId).call(
                method: "UploadCommit",
                params: ["uploadId": uploadId, "fileName": image.filename, "sessionId": chatId],
                timeoutNanoseconds: 180_000_000_000
            )
            paths.append(reply.path)
        }
        return paths
    }

    private func attachScaffoldEnvironment(controllerDeviceId: String, sandboxId: String,
                                           scope: [String: Any]) async throws -> ScaffoldEnvironmentControlResult {
        try Task.checkCancellation()
        // The engine owns authoritative provisioning and bounds each HTTP request.
        // A second mobile deadline must not abandon a healthy remote startup.
        return try await relay(for: controllerDeviceId).call(
            method: "ControlScaffoldEnvironment",
            params: ["operation": "attach", "sandbox_id": sandboxId, "scope": scope],
            timeoutNanoseconds: nil
        )
    }


    private func queueScaffoldCommand(route: ScaffoldControlRoute,
                                      payload: SessionCommandPayload,
                                      preparationGeneration: String?) async throws {
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
        var params: [String: Any] = [
            "chatId": route.projection.sessionId,
            "commandId": payload.messageId ?? UUID().uuidString.lowercased(),
            "command": command,
        ]
        if case .run = payload, let preparationGeneration {
            params["preparationGeneration"] = preparationGeneration
        }
        try Task.checkCancellation()
        // Once dispatched, admission must finish even if its view disappears.
        // Awaiting this unstructured task does not propagate sender cancellation.
        let admission = Task { @MainActor [self] in
            let _: Reply = try await relay(for: route.controllerDeviceId).call(
                method: "QueueCommand", params: params,
                timeoutNanoseconds: 30_000_000_000
            )
        }
        try await admission.value
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
    func addSessionRef(chatId: String, environment: SessionEnvironment? = nil) -> SessionRef? {
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
        guard addSessionRef(chatId: chatId) != nil else {
            throw MobileSessionError.unavailable("Couldn’t save this session’s membership")
        }
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
            if !archived {
                try doc.getMap(id: "worktreeDeletions").delete(key: chatId)
            }
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

/// Shared by live projection and demo mutations; reads return retained arrays,
/// never filter/sort the workspace during a SwiftUI body or timeline tick.
struct WorkspaceLists: Equatable {
    var overviewChats: [Chat] = []
    var settledChats: [Chat] = []
    var sharedSessionRefs: [SessionRef] = []
    var occupiedSpaces: [Space] = []
    var activeBySpace: [String: [Chat]] = [:]
    var archivedBySpace: [String: [Chat]] = [:]
    var chatsById: [String: Chat] = [:]
    var spacesById: [String: Space] = [:]
    var devicesById: [String: DeviceRow] = [:]
    var refsById: [String: SessionRef] = [:]
    var memberIds: Set<String> = []

    init(devices: [DeviceRow] = [], spaces: [Space] = [], chats: [Chat] = [], refs: [SessionRef] = []) {
        overviewChats = sessionListChats(chats, archived: false)
        settledChats = sessionListChats(chats, archived: true)
        for chat in chats { chatsById[chat.id] = chat }
        for space in spaces { spacesById[space.id] = space }
        for device in devices { devicesById[device.id] = device }
        for ref in refs {
            refsById[ref.chatId] = ref
            memberIds.insert(ref.chatId)
            if chatsById[ref.chatId] == nil { sharedSessionRefs.append(ref) }
        }
        for chat in overviewChats {
            if let id = chat.spaceId { activeBySpace[id, default: []].append(chat) }
        }
        for chat in settledChats {
            if let id = chat.spaceId { archivedBySpace[id, default: []].append(chat) }
        }
        occupiedSpaces = spaces.filter { activeBySpace[$0.id] != nil }
    }
}
