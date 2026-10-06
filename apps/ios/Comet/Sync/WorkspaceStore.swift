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
    private(set) var recoveryFailure: String?

    private(set) var doc = LoroDoc()
    private var room: RoomClient?
    private var subscriptions: [Subscription] = []
    @ObservationIgnored private var roomEpoch: UInt64 = 0
    @ObservationIgnored private var roomReadyGeneration: UInt64?
    private let config: AppConfig
    @ObservationIgnored var onProjection: (() -> Void)?
    @ObservationIgnored private var recordIntents: [String: DocDisk.RecordIntent] = [:]
    @ObservationIgnored private var recordJournalBlocked = false
    private var recordCacheId: String { config.documentCacheId(roomId: "ws4/\(config.projectScope)") }

    private func loadRecordIntents() {
        do {
            let original = try DocDisk.loadIntents([DocDisk.RecordIntent].self, id: recordCacheId) ?? []
            let unresolved = original.filter {
                !FileManager.default.fileExists(atPath: DocDisk.intentURL(for: recordCacheId).appendingPathExtension("\($0.id).outcome").path)
            }
            var intents: [DocDisk.RecordIntent] = []
            for record in unresolved {
                let normalized = try DocDisk.normalizeWorkspaceIntent(record)
                if normalized.key != record.key,
                   let canonical = unresolved.first(where: { $0.root == record.root && $0.key == normalized.key }) {
                    let target = try DocDisk.normalizeWorkspaceIntent(canonical)
                    let identical = try DocDisk.recordValue(target.before) == DocDisk.recordValue(normalized.before)
                        && DocDisk.recordValue(target.after) == DocDisk.recordValue(normalized.after)
                    let covered: Bool
                    if identical { covered = true }
                    else if let cached = DocDisk.loadReplica(id: recordCacheId) { covered = try DocDisk.canonicalCreationCoversAlias(record, canonical: canonical, cached: cached) }
                    else { covered = false }
                    guard covered else { throw MobileSessionError.unavailable("Crew has conflicting canonical and alias workspace edits; original goals are retained.") }
                    if record.id != canonical.id { try DocDisk.retainOutcome(record, id: recordCacheId, commandId: record.id) }
                    continue
                }
                intents.append(normalized)
            }
            let roots: Set<String> = ["chats", "spaces", "sessionRefs", "worktreeDeletions"]
            guard intents.count <= 1024, Set(intents.map(\.index)).count == intents.count,
                  intents.allSatisfy({ roots.contains($0.root) && !$0.key.isEmpty && UUID(uuidString: $0.id) != nil && $0.intermediates.count <= 16 }) else {
                throw MobileSessionError.unavailable("Crew workspace intent records are invalid or exceed their retention limit.")
            }
            for intent in intents where intent.root == "sessionRefs" {
                let value = try DocDisk.recordValue(intent.after) ?? DocDisk.recordValue(intent.before)
                guard let record = value?.mapValue, record["userId"]?.stringValue == config.userId,
                      let chatId = record["chatId"]?.stringValue, intent.key == sessionRefKey(chatId: chatId) else {
                    throw MobileSessionError.unavailable("Crew cannot recover a membership intent for another principal.")
                }
            }
            let encoder = JSONEncoder(); encoder.outputFormatting = [.sortedKeys]
            let originalBytes = try encoder.encode(original)
            if try encoder.encode(intents) != originalBytes {
                let evidence = DocDisk.intentURL(for: recordCacheId).appendingPathExtension("recovery")
                if !FileManager.default.fileExists(atPath: evidence.path) { try originalBytes.write(to: evidence, options: .atomic) }
                try DocDisk.saveIntents(intents, id: recordCacheId)
            }
            recordIntents = Dictionary(uniqueKeysWithValues: intents.map { ($0.index, $0) })
            recordJournalBlocked = false
        } catch { recordJournalBlocked = true; recoveryFailure = "Crew workspace intent recovery is blocked: \(error.localizedDescription). The original journal is retained." }
    }

    private func persistRecordIntents() throws {
        guard !recordJournalBlocked else { throw MobileSessionError.unavailable(recoveryFailure ?? "Crew workspace intent recovery is blocked.") }
        try DocDisk.saveIntents(Array(recordIntents.values), id: recordCacheId)
    }

    private func blockRecordRecovery(_ error: Error) {
        recoveryFailure = error.localizedDescription
        connected = false
        if let room { Task { await room.stop() } }
    }

    private func writeRecord(root: String, key: String, after: LoroValue?) throws {
        guard !recordJournalBlocked else { throw MobileSessionError.unavailable(recoveryFailure ?? "Crew workspace recovery is blocked.") }
        let before = try DocDisk.recordValue(in: doc, root: root, key: key)
        if before == after { project(); return }
        let index = root + ":" + key
        // ponytail: coalesce mutable record goals, cap at 1024 unacknowledged
        // keys; a database journal is only needed beyond this mobile ceiling.
        guard recordIntents[index] != nil || recordIntents.count < 1024 else {
            throw MobileSessionError.unavailable("Crew retains 1024 unresolved workspace edits. Recover them before editing more records.")
        }
        var intent = try recordIntents[index] ?? DocDisk.RecordIntent(root: root, key: key, before: DocDisk.recordData(before))
        if let prior = recordIntents[index] {
            guard prior.intermediates.count < 16 else {
                throw MobileSessionError.unavailable("Crew retains 16 intermediate edits to this record. Recover them before editing it again.")
            }
            intent.intermediates.append(prior.after)
        }
        intent.after = try DocDisk.recordData(after)
        intent.version = nil
        recordIntents[index] = intent
        // No CRDT operation can reach a room before its independent goal is
        // durable. This also covers termination before the debounced snapshot.
        try persistRecordIntents()
        try DocDisk.applyRecordChange(root: root, key: key, before: before, after: after, in: doc)
        doc.commit()
        intent.version = doc.oplogVv().encode()
        recordIntents[index] = intent
        try persistRecordIntents()
        project()
    }

    private func mutateRecord(root: String, key: String, _ mutate: (LoroMap) throws -> Void) throws {
        let candidate = LoroDoc()
        let row = candidate.getMap(id: "record")
        if let values = try DocDisk.recordValue(in: doc, root: root, key: key)?.mapValue {
            for (field, value) in values { try row.insert(key: field, v: value) }
        }
        try mutate(row)
        try writeRecord(root: root, key: key, after: row.getDeepValue())
    }

    private func acknowledgeRecordIntents(_ bytes: Data) {
        do {
            let version = try VersionVector.decode(bytes: bytes)
            let covered = recordIntents.filter { _, intent in
                guard let bytes = intent.version, let required = try? VersionVector.decode(bytes: bytes) else { return false }
                return version.includesVv(other: required)
            }
            for (index, intent) in covered {
                try DocDisk.retainOutcome(intent, id: recordCacheId, commandId: intent.id)
                recordIntents.removeValue(forKey: index)
            }
            if !covered.isEmpty { try persistRecordIntents() }
        } catch { blockRecordRecovery(error) }
    }

    init(config: AppConfig) {
        self.config = config
        loadRecordIntents()
    }

    @ObservationIgnored private var saver: DocSaver?

    func start() {
        guard room == nil else { return }
        roomEpoch &+= 1
        let epoch = roomEpoch
        let roomId = "ws4/\(config.projectScope)"
        let cacheId = config.documentCacheId(roomId: roomId)
        guard !recordJournalBlocked else { project(); return }
        // Hydrate locally before joining; materialize the sidebar off-main.
        if doc.oplogVv() == VersionVector() {
            if let cached = DocDisk.loadReplica(id: cacheId) { doc = cached }
            else if cacheId.hasPrefix("workspace-") {
                let legacyId = String(cacheId.dropFirst("workspace-".count))
                if let legacy = DocDisk.loadReplica(id: legacyId) {
                    do { try DocDisk.saveReplacement(doc: legacy, id: cacheId); doc = legacy }
                    catch { recoveryFailure = "Crew could not migrate the scoped workspace cache: \(error.localizedDescription)" }
                }
            }
        }
        // Project accepted goals even when the debounced cache predates them.
        // Replay in isolation before subscribing/joining; original journal IDs
        // and versions remain untouched for authoritative reconciliation.
        do {
            let migrated = doc.fork()
            if try DocDisk.migrateWorkspaceRows(in: migrated) {
                try DocDisk.retainRecoveryOriginal(doc: doc, id: cacheId)
                try DocDisk.saveReplacement(doc: migrated, id: cacheId)
                doc = migrated
            }
        } catch {
            recordJournalBlocked = true
            blockRecordRecovery(error)
            project()
            return
        }
        if !recordIntents.isEmpty {
            do {
                let local = doc.fork()
                for intent in recordIntents.values {
                    try DocDisk.applyRecordChange(root: intent.root, key: intent.key,
                        before: try DocDisk.recordValue(intent.before), after: try DocDisk.recordValue(intent.after), in: local,
                        alternatives: try intent.intermediates.map { try DocDisk.recordValue($0) })
                }
                local.commit()
                doc = local
            } catch {
                recordJournalBlocked = true
                blockRecordRecovery(error)
                project()
                return
            }
        }
        saver = DocSaver(docId: cacheId, doc: doc)
        let client = RoomClient(roomId: roomId, doc: doc, recoverApplicationIntents: !recordIntents.isEmpty) { [config] in
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
        project()
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
        guard doc === previous, let room else { return false }
        do {
            try DocDisk.retainRecoveryOriginal(doc: previous, id: config.documentCacheId(roomId: "ws4/\(config.projectScope)"))
            var pending = recordIntents
            let authoritative = replacement.oplogVv()
            _ = try DocDisk.migrateWorkspaceRows(in: replacement)
            if recordIntents.isEmpty {
                guard DocDisk.preserveLocalOperations(from: previous, in: replacement) else {
                    recoveryFailure = "Crew found conflicting workspace records. The original cache is retained."
                    return false
                }
            } else {
                // The journal is the application delta, independent of the old
                // cache's retained ancestry. Never reissue an already-covered
                // operation (including a subsequent remote tombstone).
                for (index, intent) in recordIntents {
                    if let bytes = intent.version, let required = try? VersionVector.decode(bytes: bytes),
                       authoritative.includesVv(other: required) {
                        try DocDisk.retainOutcome(intent, id: recordCacheId, commandId: intent.id)
                        pending.removeValue(forKey: index)
                        continue
                    }
                    try DocDisk.applyRecordChange(root: intent.root, key: intent.key,
                        before: try DocDisk.recordValue(intent.before), after: try DocDisk.recordValue(intent.after), in: replacement,
                        alternatives: try intent.intermediates.map { try DocDisk.recordValue($0) })
                }
                replacement.commit()
                let required = replacement.oplogVv().encode()
                for index in pending.keys { pending[index]?.version = required }
            }
            if let saver {
                try saver.replaceDocument(with: replacement, recordIntents: Array(pending.values))
            } else {
                try DocDisk.saveIntents(Array(pending.values), id: recordCacheId)
                try DocDisk.saveReplacement(doc: replacement, id: recordCacheId)
            }
            recordIntents = pending
        } catch { recoveryFailure = error.localizedDescription; return false }
        // No suspension between the final local-op merge and binding swap.
        // Keep projections, presence, relays, and all non-doc store state alive.
        subscriptions.removeAll()
        doc = replacement
        subscribeLocalUpdates(client: room)
        scheduleProjection()
        return true
    }

    /// Backgrounding hook: persist immediately.
    func flushToDisk() {
        saver?.flush()
        do { try persistRecordIntents() } catch { blockRecordRecovery(error) }
    }

    func probeSync() async { await room?.probe(force: true) }
    func retryRecovery() async {
        if recordJournalBlocked { loadRecordIntents() }
        guard !recordJournalBlocked else { return }
        recoveryFailure = nil
        stop()
        start()
    }

    func stop() {
        do { try persistRecordIntents() } catch { recoveryFailure = error.localizedDescription }
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
            recoveryFailure = nil
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
        case .recoveryBlocked(let message):
            connected = false
            roomReadyGeneration = nil
            recoveryFailure = message
        case .localChangesAcknowledged(let version):
            acknowledgeRecordIntents(version)
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
                    // Do not report initial/reconnect readiness while the UI
                    // still holds an older cached projection of this replica.
                    if let readyGeneration = self.roomReadyGeneration, generation >= readyGeneration {
                        self.connected = true
                    }
                    self.applyProjection(decoded)
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
                            environment: m["environment"]?.stringValue,
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
    func sendSessionCommand(chatId: String, payload: SessionCommandPayload,
                            admission: MobileCommandAdmission) async throws -> MobileCommandReceipt {
        guard admission.expiresAt > nowMs(), admission.scaffold == nil,
              AppConfig.canonicalSessionId(chatId) == chatId,
              let scope = admission.scope, scope.projectId == config.projectScope, scope.sessionId == chatId else {
            throw MobileSessionError.unavailable("This Crew instruction is expired or has different authority.")
        }
        guard scope.deploymentId == nil else {
            throw MobileSessionError.unavailable("This Crew instruction targets an explicit deployment. The legacy desktop command route cannot authorize it; its original draft and scope are retained. Use its scoped Scaffold authority or open it on the owner in Crew.")
        }
        guard let hostDeviceId = chat(id: chatId)?.deviceId
            ?? sessions[chatId]?.deviceId ?? admission.hostDeviceId, !hostDeviceId.isEmpty else {
            throw MobileSessionError.unavailable("This session has no known desktop host")
        }
        guard admission.hostDeviceId == hostDeviceId else {
            throw MobileSessionError.unavailable("The Crew session owner changed. The retained instruction was not sent.")
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
        let commandRelay = DeviceRelayClient(deviceId: hostDeviceId, config: config,
                                             controlSessionId: chatId, controlDeploymentId: scope.deploymentId)
        return try await commandRelay.call(
            method: "AdmitPeerCommand",
            params: [
                "chatId": chatId,
                "commandId": admission.commandId,
                "issuedAt": admission.issuedAt,
                "expiresAt": admission.expiresAt,
                "command": command,
            ]
        )
    }

    func sendScaffoldCommand(environment: SessionEnvironment,
                             payload: SessionCommandPayload,
                             admission: MobileCommandAdmission) async throws -> MobileCommandReceipt {
        guard admission.expiresAt > nowMs(), let authority = admission.scaffold,
              admission.scope == authority.environment.scope,
              authority.environment.scope == environment.scope,
              authority.environment.ownerPrincipal == environment.ownerPrincipal,
              authority.environment.source.sandboxId == environment.source.sandboxId,
              authority.environment.source.lifecycleEpoch == environment.source.lifecycleEpoch,
              authority.projection.projectId == config.projectScope,
              authority.projection.sessionId == environment.scope.sessionId,
              authority.projection.deploymentId == environment.scope.deploymentId else {
            throw MobileSessionError.unavailable("This retained Crew instruction has expired or its authority changed. Review it before sending a new instruction.")
        }
        // Revalidate the original grant on admission; never attach/resume merely
        // to retry a send or exchange a revoked grant for fresh authority.
        return try await queueScaffoldCommand(route: authority.route, payload: payload,
                                       preparationGeneration: authority.preparationGeneration,
                                       admission: admission)
    }

    func scaffoldRoute(controllerDeviceId: String, environment: SessionEnvironment) async throws -> ScaffoldControlRoute {
        guard environment.source.kind == "scaffold",
              let sandboxId = environment.source.sandboxId else {
            throw MobileSessionError.unavailable("This session has no Scaffold route")
        }
        return try await openScaffoldSession(controllerDeviceId: controllerDeviceId,
                                             sandboxId: sandboxId, scope: environment.scope,
                                             ownerPrincipal: environment.ownerPrincipal)
    }

    func openScaffoldSession(controllerDeviceId: String, sandboxId: String,
                             scope: CollaborationScope, ownerPrincipal: String? = nil) async throws -> ScaffoldControlRoute {
        guard scope.projectId == config.projectScope,
              let deploymentId = scope.deploymentId, !deploymentId.isEmpty,
              let sessionId = scope.sessionId, AppConfig.canonicalSessionId(sessionId) == sessionId else {
            throw MobileSessionError.unavailable("Open this session in Crew for its project and deployment")
        }
        let attachment = try await attachScaffoldEnvironment(
            controllerDeviceId: controllerDeviceId, sandboxId: sandboxId, scope: encodableDictionary(scope)
        )
        guard attachment.environment.source.kind == "scaffold",
              attachment.environment.source.sandboxId == sandboxId,
              attachment.environment.scope == scope,
              ownerPrincipal == nil || attachment.environment.ownerPrincipal == ownerPrincipal,
              attachment.roomProjection?.projectId == scope.projectId,
              attachment.roomProjection?.deploymentId == scope.deploymentId,
              attachment.roomProjection?.sessionId == scope.sessionId else {
            throw MobileSessionError.unavailable("Scaffold returned a different session identity")
        }
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
        // Attach resumes a paused sandbox and confirms its current host authority.
        // A second mobile deadline must not abandon a healthy remote startup.
        return try await relay(for: controllerDeviceId).call(
            method: "ControlScaffoldEnvironment",
            params: ["operation": "attach", "sandbox_id": sandboxId, "scope": scope],
            timeoutNanoseconds: nil
        )
    }


    private func queueScaffoldCommand(route: ScaffoldControlRoute,
                                      payload: SessionCommandPayload,
                                      preparationGeneration: String?,
                                      admission: MobileCommandAdmission) async throws -> MobileCommandReceipt {
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
        var params: [String: Any] = [
            "chatId": route.projection.sessionId,
            "commandId": admission.commandId,
            "issuedAt": admission.issuedAt,
            "expiresAt": admission.expiresAt,
            "command": command,
        ]
        if case .run = payload, let preparationGeneration {
            params["preparationGeneration"] = preparationGeneration
        }
        try Task.checkCancellation()
        // Once dispatched, admission must finish even if its view disappears.
        // Awaiting this unstructured task does not propagate sender cancellation.
        let admission = Task { @MainActor [self] in
            let receipt: MobileCommandReceipt = try await relay(for: route.controllerDeviceId).call(
                method: "QueueCommand", params: params,
                timeoutNanoseconds: 30_000_000_000
            )
            return receipt
        }
        return try await admission.value
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
        do {
            var addedAt: Int64 = 0
            try mutateRecord(root: "sessionRefs", key: sessionRefKey(chatId: chatId)) { row in
                try row.insert(key: "userId", v: config.userId)
                try row.insert(key: "chatId", v: chatId)
                addedAt = row.get(key: "addedAt")?.asValue()?.i64Value ?? nowMs()
                try row.insert(key: "addedAt", v: addedAt)
                if let environment, let value = LoroValue.fromEncodable(environment) { try row.insert(key: "environment", v: value) }
            }
            return SessionRef(chatId: chatId, addedAt: addedAt, environment: environment)
        } catch {
            blockRecordRecovery(error)
            return nil
        }
    }

    /// Remove only this workspace's membership; the `s2/{chatId}` room remains.
    func removeSessionRef(chatId: String) {
        do { try writeRecord(root: "sessionRefs", key: sessionRefKey(chatId: chatId), after: nil) }
        catch { blockRecordRecovery(error) }
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
        try mutateRecord(root: "chats", key: chatId) { row in
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
        }
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
            do {
                try mutateRecord(root: "spaces", key: spaceId) { row in
                    try row.insert(key: "id", v: spaceId)
                    try row.insert(key: "deviceId", v: deviceId)
                    try row.insert(key: "path", v: path)
                    try row.insert(key: "gitDetected", v: gitDetected)
                    try row.insert(key: "createdAt", v: nowMs())
                }
            } catch { blockRecordRecovery(error) }
        }
        project()
        return spaceId
    }

    func setArchived(chatId: String, archived: Bool) {
        updateChat(chatId) { row in
            try row.insert(key: "archived", v: archived)
        }
        if !archived {
            do { try writeRecord(root: "worktreeDeletions", key: chatId, after: nil) }
            catch { blockRecordRecovery(error) }
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
        guard (try? DocDisk.recordValue(in: doc, root: "chats", key: chatId)) != nil else { return }
        do { try mutateRecord(root: "chats", key: chatId, mutate) }
        catch { blockRecordRecovery(error) }
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

#if DEBUG
extension WorkspaceStore {
    static func runRecordIntentRegression() -> Bool {
        let config = AppConfig(edgeURL: URL(string: "http://127.0.0.1:1")!, mode: .dev,
            userId: "workspace-intents-\(UUID().uuidString)", projectScope: "workspace-intents",
            deviceId: "viewer", deviceName: "Crew regression")
        let cacheId = config.documentCacheId(roomId: "ws4/\(config.projectScope)")
        var stores: [WorkspaceStore] = []
        defer {
            stores.forEach { $0.stop() }
            if let files = try? FileManager.default.contentsOfDirectory(at: DocDisk.directory, includingPropertiesForKeys: nil) {
                for file in files where file.lastPathComponent.hasPrefix(cacheId) { try? FileManager.default.removeItem(at: file) }
            }
        }
        do {
            let publicId = "018eeb58-6508-78e8-a544-44682ab94c50"
            let alias = publicId + "::session::" + publicId + "::session::" + publicId
            var legacy = DocDisk.RecordIntent(root: "chats", key: alias, before: try DocDisk.recordData(.map(["id": .string(alias), "deviceId": .string("host"), "title": .string("Before")])))
            legacy.after = try DocDisk.recordData(.map(["id": .string(publicId), "deviceId": .string("host"), "title": .string("After")]))
            legacy.intermediates = [legacy.before]
            try DocDisk.saveIntents([legacy], id: cacheId)
            let migrated = WorkspaceStore(config: config)
            guard migrated.recordIntents["chats:" + publicId]?.id == legacy.id,
                  WorkspaceStore(config: config).recordIntents["chats:" + publicId]?.id == legacy.id,
                  FileManager.default.fileExists(atPath: DocDisk.intentURL(for: cacheId).appendingPathExtension("recovery").path) else { return false }
            try DocDisk.retainOutcome(legacy, id: cacheId, commandId: legacy.id)
            guard WorkspaceStore(config: config).recordIntents.isEmpty else { return false }
            let bootstrap = LoroDoc()
            for (key, clock) in [(publicId, Int64(200)), (alias, Int64(100))] {
                let row = try bootstrap.getMap(id: "chats").getOrCreateContainer(key: key, child: LoroMap())
                try row.insert(key: "id", v: key); try row.insert(key: "deviceId", v: "host")
                try row.insert(key: "title", v: "Retained user title"); try row.insert(key: "lastSeenAt", v: clock)
                try row.insert(key: "lastMessageAt", v: clock)
            }
            bootstrap.commit()
            var canonicalGoal = DocDisk.RecordIntent(root: "chats", key: publicId, before: nil)
            canonicalGoal.after = try DocDisk.recordData(DocDisk.recordValue(in: bootstrap, root: "chats", key: publicId))
            canonicalGoal.version = bootstrap.oplogVv().encode()
            var aliasGoal = DocDisk.RecordIntent(root: "chats", key: alias, before: nil)
            aliasGoal.after = try DocDisk.recordData(DocDisk.recordValue(in: bootstrap, root: "chats", key: alias))
            aliasGoal.version = canonicalGoal.version
            try DocDisk.saveReplacement(doc: bootstrap, id: cacheId)
            try DocDisk.saveIntents([canonicalGoal, aliasGoal], id: cacheId)
            let coalesced = WorkspaceStore(config: config)
            guard coalesced.recordIntents.count == 1, coalesced.recordIntents["chats:" + publicId]?.id == canonicalGoal.id,
                  WorkspaceStore(config: config).recordIntents["chats:" + publicId]?.id == canonicalGoal.id else { return false }
            var unknown = aliasGoal; unknown.id = UUID().uuidString.lowercased(); unknown.version = nil
            try DocDisk.saveIntents([canonicalGoal, unknown], id: cacheId)
            guard WorkspaceStore(config: config).recordJournalBlocked else { return false }
            let aliasRow = bootstrap.getMap(id: "chats").get(key: alias)!.asLoroMap()!
            try aliasRow.insert(key: "title", v: "Conflicting user edit"); bootstrap.commit()
            unknown.after = try DocDisk.recordData(aliasRow.getDeepValue())
            unknown.version = bootstrap.oplogVv().encode(); canonicalGoal.version = unknown.version
            try DocDisk.saveReplacement(doc: bootstrap, id: cacheId)
            try DocDisk.saveIntents([canonicalGoal, unknown], id: cacheId)
            guard WorkspaceStore(config: config).recordJournalBlocked else { return false }
            try DocDisk.saveIntents([DocDisk.RecordIntent](), id: cacheId)
            let existing = UUID().uuidString.lowercased()
            let created = UUID().uuidString.lowercased()
            let source = LoroDoc()
            let row = try source.getMap(id: "chats").getOrCreateContainer(key: existing, child: LoroMap())
            try row.insert(key: "id", v: existing); try row.insert(key: "deviceId", v: "host")
            try row.insert(key: "title", v: "Original")
            let membership = try source.getMap(id: "sessionRefs").getOrCreateContainer(
                key: "\(config.userId.utf8.count):\(config.userId):\(existing)", child: LoroMap())
            try membership.insert(key: "userId", v: config.userId)
            try membership.insert(key: "chatId", v: existing)
            try membership.insert(key: "addedAt", v: Int64(1))
            let removedSpace = try source.getMap(id: "spaces").getOrCreateContainer(key: "removed", child: LoroMap())
            try removedSpace.insert(key: "id", v: "removed")
            try removedSpace.insert(key: "deviceId", v: "host")
            try removedSpace.insert(key: "path", v: "/removed")
            source.commit()
            DocDisk.save(doc: source, id: cacheId)
            let offline = WorkspaceStore(config: config)
            stores.append(offline)
            _ = try offline.doc.importWith(bytes: source.export(mode: .snapshot), origin: "workspace-intent-regression")
            offline.rename(chatId: existing, title: "Intermediate offline title")
            offline.rename(chatId: existing, title: "Final offline title")
            try offline.mutateRecord(root: "chats", key: created) { row in
                try row.insert(key: "id", v: created); try row.insert(key: "deviceId", v: "host")
                try row.insert(key: "title", v: "Created offline")
                try row.insert(key: "config", v: LoroValue.fromJSON(["nested": ["sandbox": "workspace-write"]]))
            }
            try offline.mutateRecord(root: "spaces", key: "created-space") { row in
                try row.insert(key: "id", v: "created-space")
                try row.insert(key: "deviceId", v: "host")
                try row.insert(key: "path", v: "/accepted")
                try row.insert(key: "name", v: "Accepted offline space")
            }
            try offline.writeRecord(root: "spaces", key: "removed", after: nil)
            guard offline.addSessionRef(chatId: created) != nil,
                  offline.recoveryFailure == nil else { return false }
            // Restart before the debounced snapshot ever ran. The only cache
            // still has the original title; accepted goals are independent.
            let restarted = WorkspaceStore(config: config)
            stores.append(restarted)
            guard restarted.recordIntents.count == 5,
                  let cached = DocDisk.loadReplica(id: cacheId),
                  try DocDisk.recordValue(in: cached, root: "chats", key: existing)?.mapValue?["title"]?.stringValue == "Original" else { return false }
            let journal = try Data(contentsOf: DocDisk.intentURL(for: cacheId))
            restarted.start()
            let local = restarted.doc
            guard !restarted.connected, restarted.recoveryFailure == nil,
                  restarted.chat(id: existing)?.title == "Final offline title",
                  restarted.chat(id: created)?.title == "Created offline",
                  restarted.sessionRef(id: created) != nil,
                  restarted.space(id: "created-space")?.name == "Accepted offline space",
                  restarted.space(id: "removed") == nil,
                  try Data(contentsOf: DocDisk.intentURL(for: cacheId)) == journal else { return false }
            // An earlier edit was admitted under another operation identity.
            // Its successor must rebase, not overwrite a remote disjoint field.
            try row.insert(key: "title", v: "Intermediate offline title")
            try row.insert(key: "archived", v: true)
            source.commit()
            let floor = source.stateFrontiers()
            try source.getMap(id: "meta").insert(key: "current", v: true); source.commit()
            guard let replacement = DocDisk.replacementSnapshot(bytes: try source.export(mode: .shallowSnapshot(frontiers: floor))),
                  restarted.adoptSnapshot(previous: local, replacement: replacement),
                  let records = replacement.getDeepValue().mapValue?["chats"]?.mapValue,
                  records[existing]?.mapValue?["title"]?.stringValue == "Final offline title",
                  records[existing]?.mapValue?["archived"]?.boolValue == true,
                  records[created]?.mapValue?["config"]?.mapValue?["nested"]?.mapValue?["sandbox"]?.stringValue == "workspace-write",
                  try DocDisk.recordValue(in: replacement, root: "sessionRefs", key: restarted.sessionRefKey(chatId: created))?.mapValue?["userId"]?.stringValue == config.userId else { return false }
            restarted.acknowledgeRecordIntents(replacement.oplogVv().encode())
            let afterAck = WorkspaceStore(config: config)
            stores.append(afterAck)
            guard afterAck.recordIntents.isEmpty,
                  try DocDisk.recordValue(in: DocDisk.loadReplica(id: cacheId)!, root: "chats", key: existing)?.mapValue?["title"]?.stringValue == "Final offline title" else { return false }
            restarted.rename(chatId: existing, title: "Retained conflicting title")
            try row.insert(key: "title", v: "Authoritative conflicting title"); source.commit()
            guard let conflict = DocDisk.replacementSnapshot(bytes: try source.export(mode: .shallowSnapshot(frontiers: source.stateFrontiers()))),
                  !restarted.adoptSnapshot(previous: replacement, replacement: conflict),
                  restarted.doc === replacement, restarted.recoveryFailure != nil,
                  let retained = try DocDisk.loadIntents([DocDisk.RecordIntent].self, id: cacheId),
                  retained.contains(where: { (try? DocDisk.recordValue($0.after))?.mapValue?["title"]?.stringValue == "Retained conflicting title" }) else { return false }
            E2ERunner.log("OK Crew workspace intents: offline start projects stale-cache rename, space creation, membership and deletion without rewriting the journal; shallow recovery, durable ACK, conflict originals retained")
            return true
        } catch { E2ERunner.log("FAIL Crew workspace intents: \(error)"); return false }
    }
}
#endif
