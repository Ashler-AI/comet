// Session doc mirror — transcript entries for one chat (crates/doc/src/schema.rs).
// Commands go through the authenticated host RPC; the phone never writes the
// durable command ledger. Optimistic echoes keep their client-minted message
// ids until the host materializes them, or admission fails.

import Foundation
import Loro
import Observation

@MainActor
@Observable
final class SessionStore {
    let chatId: String
    private(set) var deploymentId: String?
    private(set) var entries: [MessageEntry] = []
    private(set) var publishedSession: SessionRow?
    private(set) var publishedEnvironment: SessionEnvironment?
    private(set) var previewTitle: String?
    @ObservationIgnored private var metadataOnly: Bool
    private(set) var transcriptActivity: SessionRow?
    /// Bumped on every change to `entries` / `pendingSends`. The transcript's
    /// row builder memoizes on it, so a body re-eval that was triggered by
    /// something else (scrolling) costs O(1) instead of re-deriving every row.
    private(set) var revision: UInt64 = 0
    /// Whether this chat's transcript has already been revealed once.
    ///
    /// Lives on the store, not the view: the reveal gate is `@State`, so any
    /// re-creation of TranscriptView reset it to "hidden" and blanked an
    /// already-visible transcript until the settle loop finished. The store is
    /// cached per chat, so it outlives that churn.
    @ObservationIgnored var hasRevealed = false
    @ObservationIgnored let transcriptBuilder = TranscriptBuilderCache()
    private(set) var connected = false
    /// Client-minted ids of sends the host hasn't materialized yet.
    private(set) var pendingSends: [(messageId: String, text: String, at: Int64)] = []
    private(set) var sendFailure: String?
    private(set) var failedPrompt: String?
    private(set) var failedImages: [MobileImageAttachment] = []
    private(set) var sending = false
    @ObservationIgnored var attachmentUploader: (([MobileImageAttachment]) async throws -> [String])?
    @ObservationIgnored private var uploadedImages: [UUID: String] = [:]
    @ObservationIgnored private var submittedDrafts: [String: SubmittedDraft] = [:]
    @ObservationIgnored private var retryDraft: SubmittedDraft?
    @ObservationIgnored private var retryIsTerminal = false

    private struct SubmittedDraft {
        var messageId: String
        var prompt: String
        var images: [MobileImageAttachment]
        var steer: Bool
        var payload: SessionCommandPayload?
    }

    private(set) var doc = LoroDoc()
    private var room: RoomClient?
    private var subscriptions: [Subscription] = []
    @ObservationIgnored private var roomEpoch: UInt64 = 0
    @ObservationIgnored private var lastRemoteUpdateAt: Int64?
    private let config: AppConfig

    /// Demo mode: no room, entries driven externally.
    private let offline: Bool
    /// Demo hook: invoked instead of the command plane when offline.
    @ObservationIgnored var demoResponder: ((String) -> Void)?
    /// The trusted desktop host/controller admits every command. Transport and
    /// admission errors are handled here alongside the matching optimistic echo.
    @ObservationIgnored var commandSender: ((SessionCommandPayload) async throws -> Void)?

    init(chatId: String, config: AppConfig, deploymentId: String? = nil, offline: Bool = false,
         metadataOnly: Bool = false) {
        self.chatId = chatId
        self.config = config
        self.deploymentId = deploymentId
        self.offline = offline
        self.metadataOnly = metadataOnly
    }

    /// Demo-mode injection point (also used by previews).
    func setEntries(_ new: [MessageEntry]) {
        guard entries != new else { return }
        invalidateProjection()
        entries = new
        previewTitle = Self.titlePreview(in: new)
        transcriptActivity = Self.activity(in: new, chatId: chatId, observedAt: nowMs())
        revision &+= 1
    }

    func activateTranscript() {
        guard metadataOnly else { return }
        metadataOnly = false
        invalidateProjection()
        project()
    }

    @ObservationIgnored private var saver: DocSaver?
    @ObservationIgnored private var hydrationTask: Task<LoroDoc?, Never>?
    @ObservationIgnored private(set) var hydrationComplete = false
    var isHydrating: Bool { hydrationTask != nil }

    func start() {
        guard room == nil, hydrationTask == nil, !offline else { return }
        roomEpoch &+= 1
        invalidateProjection()
        let epoch = roomEpoch
        let cacheId = config.documentCacheId(roomId: chatId, deploymentId: deploymentId)
        // Restarts retain their already-hydrated replica. Never re-import a
        // potentially older cache over the live transcript.
        guard !hydrationComplete else {
            joinRoom(cacheId: cacheId, epoch: epoch)
            return
        }
        let previous = doc
        let load = Task.detached(priority: .userInitiated) {
            DocDisk.loadReplica(id: cacheId)
        }
        hydrationTask = load
        Task { @MainActor [weak self] in
            let cached = await load.value
            guard let self, self.roomEpoch == epoch, self.doc === previous,
                  !load.isCancelled else { return }
            self.hydrationTask = nil
            // Optimistic sends do not write the doc. Preserve any actual local
            // operations that did land while the isolated cache was importing.
            if let cached, DocDisk.preserveLocalOperations(from: previous, in: cached) {
                self.invalidateProjection()
                self.doc = cached
            }
            self.hydrationComplete = true
            self.joinRoom(cacheId: cacheId, epoch: epoch)
        }
    }

    private func joinRoom(cacheId: String, epoch: UInt64) {
        guard roomEpoch == epoch, room == nil else { return }
        saver = DocSaver(docId: cacheId, doc: doc)
        let client = RoomClient(roomId: chatId, doc: doc) { [config, chatId, deploymentId] in
            await config.sessionSocketURL(chatId: chatId, deploymentId: deploymentId)
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
        Task { @MainActor [weak self] in
            guard let self, self.roomEpoch == epoch else { return }
            await client.start()
        }
        project()
    }

    private func subscribeLocalUpdates(client: RoomClient) {
        let epoch = roomEpoch
        let subscribedDoc = doc
        subscriptions.append(doc.subscribeLocalUpdate { [weak client, weak self, weak subscribedDoc] update in
            guard let client else { return }
            let bytes = [UInt8](update)
            Task { await client.sendLocalUpdate(bytes) }
            Task { @MainActor [weak self, weak subscribedDoc] in
                guard let self, let subscribedDoc, self.roomEpoch == epoch,
                      self.doc === subscribedDoc else { return }
                self.saver?.poke()
            }
        })
    }

    private func adoptSnapshot(previous: LoroDoc, replacement: LoroDoc) -> Bool {
        guard doc === previous, let room,
              DocDisk.preserveLocalOperations(from: previous, in: replacement) else { return false }
        // Keep entries visible until the replacement projection is ready, and
        // retain optimistic sends, command admission, and the reveal state.
        invalidateProjection()
        subscriptions.removeAll()
        doc = replacement
        subscribeLocalUpdates(client: room)
        saver?.replaceDocument(with: replacement)
        project()
        return true
    }

    /// Backgrounding hook: persist immediately.
    func flushToDisk() {
        saver?.flush()
    }

    func stop() {
        roomEpoch &+= 1
        hydrationTask?.cancel()
        hydrationTask = nil
        invalidateProjection()
        subscriptions.removeAll()
        saver?.flush()
        saver = nil
        if let room {
            Task { await room.stop() }
        }
        room = nil
        connected = false
    }

    func updateDeploymentId(_ value: String?) {
        guard deploymentId != value else { return }
        deploymentId = value
        guard !offline else { return }
        stop()
        doc = LoroDoc()
        hydrationComplete = false
        if !entries.isEmpty {
            entries = []
            revision &+= 1
        }
        publishedSession = nil
        publishedEnvironment = nil
        previewTitle = nil
        transcriptActivity = nil
        lastRemoteUpdateAt = nil
        hasRevealed = false
        transcriptBuilder.reset()
        uploadedImages.removeAll()
        start()
    }

    private func handle(_ event: RoomEvent) {
        switch event {
        case .connected:
            connected = true
            project()
        case .disconnected:
            connected = false
        case .remoteUpdate:
            lastRemoteUpdateAt = nowMs()
            project()
            saver?.poke()
        case .ephemeralUpdate:
            break
        }
    }

    // MARK: Projection

    @ObservationIgnored private var projectionTask: Task<ProjectionResult?, Never>?
    @ObservationIgnored private var projectPending = false
    @ObservationIgnored private var projectionEpoch: UInt64 = 0
    @ObservationIgnored private var lastProjectionKey: ProjectionKey?
    @ObservationIgnored private(set) var projectionCount: UInt64 = 0
    var isProjecting: Bool { projectionTask != nil }

    private struct ProjectionKey: Equatable {
        var version: VersionVector
        var metadataOnly: Bool
        var observedAt: Int64?
        var pendingMessageIds: Set<String>
    }

    private struct ProjectionResult {
        var key: ProjectionKey
        var decoded: Projection?
        var entriesChanged: Bool
        var failures: [String: String]
    }

    private func invalidateProjection() {
        projectionEpoch &+= 1
        projectionTask?.cancel()
        projectionTask = nil
        projectPending = false
        lastProjectionKey = nil
    }

    /// Container reads and decoding stay off-main. Streaming bursts coalesce;
    /// unchanged version/inputs skip materialization altogether. Internal for
    /// the benchmark runner to measure the same path the room uses.
    func project() {
        guard hydrationTask == nil else { return }
        guard projectionTask == nil else {
            projectPending = true
            return
        }
        let doc = self.doc
        let epoch = roomEpoch
        let generation = projectionEpoch
        let pendingMessageIds = Set(pendingSends.map(\.messageId))
        let chatId = self.chatId
        let observedAt = lastRemoteUpdateAt
        let metadataOnly = self.metadataOnly
        let previousKey = lastProjectionKey
        let previousEntries = entries
        let work = Task.detached(priority: .userInitiated) { () -> ProjectionResult? in
            guard !Task.isCancelled else { return nil }
            let key = ProjectionKey(version: doc.stateVv(), metadataOnly: metadataOnly,
                                    observedAt: metadataOnly ? nil : observedAt,
                                    pendingMessageIds: pendingMessageIds)
            if key == previousKey {
                return ProjectionResult(key: key, decoded: nil, entriesChanged: false, failures: [:])
            }
            let decoded = metadataOnly
                ? Self.decodeMetadata(from: doc, chatId: chatId)
                : Self.decodeProjection(from: doc, chatId: chatId, observedAt: observedAt)
            guard !Task.isCancelled else { return nil }
            let failures = pendingMessageIds.isEmpty || metadataOnly ? [:]
                : Self.commandFailures(from: doc.getList(id: "commands").getDeepValue().listValue ?? [],
                                       messageIds: pendingMessageIds)
            return ProjectionResult(key: key, decoded: decoded,
                                    entriesChanged: !metadataOnly && decoded.entries != previousEntries,
                                    failures: failures)
        }
        projectionTask = work
        Task { @MainActor [weak self] in
            let result = await work.value
            guard let self, self.roomEpoch == epoch, self.projectionEpoch == generation,
                  self.doc === doc, self.metadataOnly == metadataOnly, !work.isCancelled else { return }
            self.projectionTask = nil
            // A multi-container read can straddle a remote import. Do not
            // publish that mixed view, or let it resolve an optimistic echo.
            guard let result, doc.stateVv() == result.key.version else {
                self.projectPending = false
                self.project()
                return
            }
            self.lastProjectionKey = result.key
            if let decoded = result.decoded {
                self.projectionCount &+= 1
                self.apply(decoded, metadataOnly: metadataOnly, entriesChanged: result.entriesChanged)
            }
            for (messageId, failure) in result.failures
                where self.pendingSends.contains(where: { $0.messageId == messageId }) {
                self.reportSendFailure(failure, messageId: messageId, terminal: true)
            }
            if self.projectPending {
                self.projectPending = false
                self.project()
            }
        }
    }

    private func apply(_ decoded: Projection, metadataOnly: Bool, entriesChanged: Bool) {
        if publishedSession != decoded.session { publishedSession = decoded.session }
        if publishedEnvironment != decoded.environment { publishedEnvironment = decoded.environment }
        if previewTitle != decoded.previewTitle { previewTitle = decoded.previewTitle }
        // Metadata projections never own history or optimistic sends, including
        // a metadata result queued immediately before transcript activation.
        guard !metadataOnly else { return }
        if entriesChanged { entries = decoded.entries }
        if transcriptActivity != decoded.activity { transcriptActivity = decoded.activity }
        let pendingCount = pendingSends.count
        if !pendingSends.isEmpty || !submittedDrafts.isEmpty {
            let ids = Set(entries.map(\.id))
            pendingSends.removeAll { ids.contains($0.messageId) }
            for id in ids {
                if let draft = submittedDrafts.removeValue(forKey: id) {
                    for image in draft.images { uploadedImages.removeValue(forKey: image.id) }
                }
            }
        }
        if entriesChanged || pendingSends.count != pendingCount { revision &+= 1 }
    }

    /// Only reconcile this phone's in-flight echoes. Historical commands are
    /// neither replayed nor modified, including malformed legacy mobile rows.
    nonisolated private static func commandFailures(
        from commands: [LoroValue], messageIds: Set<String>
    ) -> [String: String] {
        guard !messageIds.isEmpty else { return [:] }
        var failures: [String: String] = [:]
        for value in commands {
            guard let command = value.mapValue,
                  let status = command["status"]?.stringValue,
                  ["rejected", "expired", "superseded", "cancelled"].contains(status),
                  let payload = command["payload"]?.mapValue else { continue }
            let messageId = payload["messageId"]?.stringValue
                ?? payload["action"]?.mapValue?["message_id"]?.stringValue
            guard let messageId, messageIds.contains(messageId) else { continue }
            failures[messageId] = command["resolution"]?.stringValue
                ?? "The desktop marked this message \(status)"
        }
        return failures
    }

    /// Materialize only transcript messages; never commands or publications.
    nonisolated static func decodeEntries(from doc: LoroDoc) -> [MessageEntry]? {
        guard let messages = doc.getList(id: "messages").getDeepValue().listValue else { return nil }
        return joinContinuations(messages.compactMap(entryFrom))
    }

    struct Projection {
        var entries: [MessageEntry]
        var session: SessionRow?
        var activity: SessionRow?
        var environment: SessionEnvironment?
        var previewTitle: String?
    }

    nonisolated static func decodeProjection(
        from doc: LoroDoc, chatId: String, observedAt: Int64?
    ) -> Projection {
        let raw = (doc.getList(id: "messages").getDeepValue().listValue ?? []).compactMap(entryFrom)
        var decoded = decodePublication(from: doc, chatId: chatId)
        decoded.entries = joinContinuations(raw)
        decoded.activity = activity(in: raw, chatId: chatId, observedAt: observedAt)
        decoded.previewTitle = titlePreview(in: raw)
        return decoded
    }

    nonisolated private static func decodePublication(from doc: LoroDoc, chatId: String) -> Projection {
        var session: SessionRow?
        var environment: SessionEnvironment?
        let publications = doc.getList(id: "publications")
        for index in (0..<publications.len()).reversed() {
            guard let item = publications.get(index: index) else { continue }
            let recordItem = item.asLoroMap()?.get(key: "record")
            let record = item.asValue()?.mapValue?["record"] ?? recordItem?.asValue()
            let recordMap = recordItem?.asLoroMap()
            let kind = record?.mapValue?["kind"]?.stringValue
                ?? recordMap?.get(key: "kind")?.asValue()?.stringValue
            guard kind == "agentSession" else { continue }
            let valueItem = recordMap?.get(key: "value")
            let scalarValue = record?.mapValue?["value"] ?? valueItem?.asValue()
            let valueMap = valueItem?.asLoroMap()
            let publishedChatId = scalarValue?.mapValue?["chatId"]?.stringValue
                ?? valueMap?.get(key: "chatId")?.asValue()?.stringValue
            guard publishedChatId == chatId,
                  let value = (scalarValue ?? valueMap?.getDeepValue())?.mapValue,
                  let deviceId = value["ownerDeviceId"]?.stringValue,
                  let createdAt = value["createdAt"]?.i64Value else { continue }
            let updatedAt = value["updatedAt"]?.i64Value ?? createdAt
            session = SessionRow(
                chatId: chatId, deviceId: deviceId,
                status: value["status"]?.stringValue.flatMap(SessionStatus.init(rawValue:)) ?? .idle,
                startedAt: updatedAt, updatedAt: updatedAt
            )
            if let value = value["environment"],
               let data = try? JSONSerialization.data(withJSONObject: value.jsonObject) {
                environment = try? JSONDecoder().decode(SessionEnvironment.self, from: data)
            }
            break
        }
        return Projection(entries: [], session: session, activity: nil,
                          environment: environment, previewTitle: nil)
    }

    nonisolated private static func titlePreview(in entries: [MessageEntry]) -> String? {
        guard let first = entries.first(where: { $0.role == .user && !$0.isPeerMessage && $0.continuationOf == nil }) else { return nil }
        let text = first.parts.compactMap { part -> String? in
            if case .text(_, let text) = part { return text }
            return nil
        }.joined(separator: " ")
        guard let title = normalizedSessionTitle(text) else { return nil }
        return String(title.prefix(48)) + (title.count > 48 ? "…" : "")
    }

    nonisolated private static func decodeMetadata(from doc: LoroDoc, chatId: String) -> Projection {
        var decoded = decodePublication(from: doc, chatId: chatId)
        let messages = doc.getList(id: "messages")
        for index in 0..<messages.len() {
            guard let item = messages.get(index: index) else { continue }
            // Check the scalar role before converting a possibly large tool row.
            let map = item.asLoroMap()
            let value = item.asValue()
            let role = value?.mapValue?["role"]?.stringValue ?? map?.get(key: "role")?.asValue()?.stringValue
            guard role == "user", let value = value ?? map?.getDeepValue(),
                  let entry = entryFrom(value), !entry.isPeerMessage, entry.continuationOf == nil else { continue }
            decoded.previewTitle = titlePreview(in: [entry])
            break
        }
        return decoded
    }

    /// Read the raw tail, not joined roots: a terminal continuation can finish a
    /// root that is still stamped streaming, and older turns must never win.
    nonisolated private static func activity(
        in raw: [MessageEntry], chatId: String, observedAt: Int64?
    ) -> SessionRow? {
        guard let entry = raw.last(where: { $0.role == .assistant }),
              let status = entry.status else { return nil }
        return SessionRow(
            chatId: chatId, deviceId: entry.deviceId,
            status: status == .streaming ? .working : .idle,
            startedAt: entry.createdAt,
            updatedAt: status == .streaming ? max(entry.createdAt, observedAt ?? entry.createdAt) : entry.createdAt
        )
    }

    nonisolated private static func entryFrom(_ value: LoroValue) -> MessageEntry? {
        guard let m = value.mapValue,
              let id = m["id"]?.stringValue,
              let roleStr = m["role"]?.stringValue,
              let role = MessageRole(rawValue: roleStr) else { return nil }
        let parts = (m["parts"]?.listValue ?? []).compactMap(partFrom)
        return MessageEntry(id: id, role: role, parts: parts,
                            createdAt: m["createdAt"]?.i64Value ?? 0,
                            deviceId: m["deviceId"]?.stringValue ?? "",
                            status: m["status"]?.stringValue.flatMap(MessageStatus.init(rawValue:)),
                            continuationOf: m["continuationOf"]?.stringValue,
                            peerMessage: peerMessageFrom(m["peerMessage"]))
    }

    nonisolated private static func peerMessageFrom(_ value: LoroValue?) -> PeerMessageProvenance? {
        guard let map = value?.mapValue,
              let commandId = map["commandId"]?.stringValue,
              let sourceChatId = map["sourceChatId"]?.stringValue,
              let threadId = map["threadId"]?.stringValue else { return nil }
        let replyTo: String?
        switch map["replyTo"] {
        case nil, .some(.null): replyTo = nil
        case .some(.string(let value)): replyTo = value
        default: return nil
        }
        let provenance = PeerMessageProvenance(commandId: commandId, sourceChatId: sourceChatId,
                                               threadId: threadId, replyTo: replyTo)
        return provenance.isValid ? provenance : nil
    }

    nonisolated private static func partFrom(_ value: LoroValue) -> MessagePart? {
        guard let m = value.mapValue,
              let id = m["id"]?.stringValue,
              let kind = m["kind"]?.stringValue else { return nil }
        switch kind {
        case "text":
            return .text(id: id, text: m["text"]?.stringValue ?? "")
        case "tool":
            guard let callMap = m["call"]?.mapValue else { return nil }
            let tag = callMap["kind"]?.stringValue ?? "unknown"
            var fields: [String: AnyHashable] = [:]
            for (k, v) in callMap where k != "kind" {
                if let s = v.stringValue { fields[k] = s }
                else if let b = v.boolValue { fields[k] = b }
                else if let i = v.i64Value { fields[k] = i }
                else if let list = v.listValue {
                    // ApplyPatch changes / Todo items — keep a JSON echo.
                    fields[k] = list.map { "\($0.jsonObject)" }
                }
            }
            // isError presence IS the resolution marker (schema.rs:96).
            let isError = m["isError"]?.boolValue
            return .tool(id: id, call: RenderToolCall(tag: tag, fields: fields),
                         isError: isError ?? false, resolved: isError != nil)
        case "input":
            var questions: [UserInputQuestion] = []
            if let list = m["questions"]?.listValue,
               let data = try? JSONSerialization.data(withJSONObject: list.map(\.jsonObject)),
               let decoded = try? JSONDecoder().decode([UserInputQuestion].self, from: data) {
                questions = decoded
            }
            return .input(id: id, requestId: id, questions: questions,
                          resolved: m["resolved"]?.boolValue ?? false)
        case "error":
            return .error(id: id, message: m["message"]?.stringValue ?? "")
        default:
            return nil
        }
    }

    /// schema.rs join_continuation_entries: concatenate continuation parts onto
    /// the root in list order; orphans surface standalone.
    nonisolated static func joinContinuations(_ raw: [MessageEntry]) -> [MessageEntry] {
        var roots: [MessageEntry] = []
        var index: [String: Int] = [:]
        for entry in raw {
            if let rootId = entry.continuationOf, let ix = index[rootId],
               roots[ix].role == entry.role,
               !roots[ix].isPeerMessage || entry.peerMessage == roots[ix].peerMessage {
                roots[ix].parts.append(contentsOf: entry.parts)
            } else {
                index[entry.id] = roots.count
                roots.append(entry)
            }
        }
        return roots
    }

    // MARK: Derived

    var lastEntryId: String? { entries.last?.id }

    var liveEntry: MessageEntry? {
        entries.last(where: { $0.status == .streaming })
    }

    /// The unresolved input request to surface in the question panel.
    var openInputRequest: (entryId: String, requestId: String, questions: [UserInputQuestion])? {
        for entry in entries.reversed() {
            for part in entry.parts.reversed() {
                // An empty question list can't be answered, so it must not take
                // the composer's place — leaving the user with no way to type.
                if case .input(_, let requestId, let questions, let resolved) = part,
                   !resolved, !questions.isEmpty {
                    return (entry.id, requestId, questions)
                }
            }
        }
        return nil
    }

    // MARK: Command plane (authenticated desktop admission)

    @discardableResult
    func sendRun(prompt: String, chat: Chat?, images: [MobileImageAttachment] = []) async -> Bool {
        await sendMessage(prompt: prompt, chat: chat, images: images, steer: false)
    }

    @discardableResult
    func sendSteer(prompt: String, images: [MobileImageAttachment] = []) async -> Bool {
        await sendMessage(prompt: prompt, chat: nil, images: images, steer: true)
    }

    private func sendMessage(prompt: String, chat: Chat?, images: [MobileImageAttachment], steer: Bool) async -> Bool {
        guard !sending, !prompt.isEmpty || !images.isEmpty else { return false }
        sending = true
        defer { sending = false }
        clearSendFailure()
        let retry = retryDraft.flatMap {
            $0.prompt == prompt && $0.images.map(\.id) == images.map(\.id) ? $0 : nil
        }
        if let retry, entries.contains(where: { $0.id == retry.messageId }) {
            retryDraft = nil
            return true
        }
        var draft = retry ?? SubmittedDraft(messageId: UUID().uuidString.lowercased(), prompt: prompt,
                                            images: images, steer: steer)
        if retry != nil, retryIsTerminal {
            draft.messageId = UUID().uuidString.lowercased()
            draft.payload = nil
            draft.steer = steer
        }
        submittedDrafts[draft.messageId] = draft
        do {
            let missing = images.filter { uploadedImages[$0.id] == nil }
            if !missing.isEmpty {
                guard let attachmentUploader else {
                    throw MobileSessionError.unavailable("This session has no available image upload route.")
                }
                let paths = try await attachmentUploader(missing)
                guard paths.count == missing.count,
                      paths.allSatisfy({ $0.hasPrefix("/") && !$0.contains("\n") && !$0.contains("\r") }) else {
                    throw MobileSessionError.unavailable("The host did not commit every attached image.")
                }
                for (image, path) in zip(missing, paths) { uploadedImages[image.id] = path }
            }
            try Task.checkCancellation()
            let paths = images.compactMap { uploadedImages[$0.id] }
            let content = MobileImageAttachment.prompt(prompt, paths: paths)
            if offline {
                demoResponder?(content)
                submittedDrafts.removeValue(forKey: draft.messageId)
                retryDraft = nil
                return true
            }
            guard let commandSender else {
                throw MobileSessionError.unavailable("This session has no available desktop command route")
            }
            let payload: SessionCommandPayload
            if let retained = draft.payload {
                payload = retained
            } else if draft.steer {
                payload = .steer(prompt: content, messageId: draft.messageId)
            } else {
                var request = RunRequest(prompt: content,
                                         model: chat?.config?.model,
                                         reasoning: chat?.config?.reasoning,
                                         cwd: chat?.cwd ?? "",
                                         sandbox: chat?.config?.sandbox ?? "workspace-write")
                request.attachments = paths
                payload = .run(request: request, messageId: draft.messageId)
            }
            draft.payload = payload
            submittedDrafts[draft.messageId] = draft
            stagePendingSend(prompt: content, messageId: draft.messageId)
            try await commandSender(payload)
            // Projection may report a rejection while admission is suspended.
            guard retryDraft?.messageId != draft.messageId || sendFailure == nil else { return false }
            retryDraft = nil
            return true
        } catch {
            // A lost RPC response must not turn an already materialized send
            // into a second user message on deliberate retry.
            if entries.contains(where: { $0.id == draft.messageId }) { return true }
            if retryDraft?.messageId == draft.messageId, sendFailure != nil { return false }
            reportSendFailure(error is CancellationError ? "Send cancelled. Your draft is still here." : error.localizedDescription,
                              messageId: draft.messageId)
            return false
        }
    }

    @discardableResult
    func stagePendingSend(prompt: String, messageId: String = UUID().uuidString.lowercased()) -> String {
        pendingSends.append((messageId, prompt, nowMs()))
        revision &+= 1
        return messageId
    }

    func dropPendingSend(messageId: String) {
        let count = pendingSends.count
        pendingSends.removeAll { $0.messageId == messageId }
        if pendingSends.count != count { revision &+= 1 }
    }

    func reportSendFailure(_ message: String, messageId: String?, terminal: Bool = false) {
        if let messageId, let draft = submittedDrafts.removeValue(forKey: messageId) {
            retryDraft = draft
            retryIsTerminal = terminal
            failedPrompt = draft.prompt
            failedImages = draft.images
        } else {
            failedPrompt = pendingSends.first(where: { $0.messageId == messageId })?.text
        }
        if let messageId { dropPendingSend(messageId: messageId) }
        sendFailure = message
    }

    func clearSendFailure() {
        sendFailure = nil
        failedPrompt = nil
        failedImages = []
    }

    func sendInterrupt() {
        guard !offline else { return }
        sendCommand(.interrupt)
    }

    func respondInput(requestId: String, answers: [UserInputAnswer]) {
        guard !offline else { return }
        sendCommand(.respondInput(requestId: requestId, answers: answers))
    }

    private func sendCommand(_ payload: SessionCommandPayload) {
        guard let commandSender else {
            reportSendFailure("This session has no available desktop command route",
                              messageId: payload.messageId)
            return
        }
        Task { @MainActor in
            do {
                try await commandSender(payload)
            } catch {
                reportSendFailure(error.localizedDescription, messageId: payload.messageId)
            }
        }
    }
}
