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
        entries = new
        previewTitle = Self.titlePreview(in: new)
        transcriptActivity = Self.activity(in: new, chatId: chatId, observedAt: nowMs())
        revision &+= 1
    }

    func activateTranscript() {
        guard metadataOnly else { return }
        metadataOnly = false
        project()
    }

    @ObservationIgnored private var saver: DocSaver?

    func start() {
        guard room == nil, !offline else { return }
        roomEpoch &+= 1
        let epoch = roomEpoch
        // Local-first: last-synced transcript renders instantly (even when the
        // host device is offline); the join backfills incrementally from here.
        let cacheId = config.documentCacheId(roomId: chatId, deploymentId: deploymentId)
        if DocDisk.load(into: doc, id: cacheId) {
            project()
        }
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
        Task { await client.start() }
        project()
    }

    private func subscribeLocalUpdates(client: RoomClient) {
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
        // Keep entries visible until the replacement projection is ready, and
        // retain optimistic sends, command admission, and the reveal state.
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
        subscriptions.removeAll()
        saver?.flush()
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
        entries = []
        publishedSession = nil
        publishedEnvironment = nil
        previewTitle = nil
        transcriptActivity = nil
        lastRemoteUpdateAt = nil
        hasRevealed = false
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

    /// In-flight guard + trailing re-run for the off-main projection below.
    @ObservationIgnored private var projecting = false
    @ObservationIgnored private var projectPending = false

    /// Re-derive `entries` from the doc, off the main thread.
    ///
    /// `getDeepValue()` materializes the WHOLE doc and the decode walks every
    /// message and every part, so this is O(transcript) — tens of ms on a big
    /// session, and it runs on every remote update. On the main actor that
    /// stalled the first frame of a cached session and janked streaming.
    /// Reading the doc from a background task is the access class the design
    /// already has: `RoomClient` is a non-main actor that imports into this
    /// same doc, so it is concurrently read/written today regardless.
    ///
    /// Overlapping calls coalesce to a single trailing re-run — a streaming
    /// burst must not queue one whole-doc projection per token.
    private func project() {
        guard !projecting else {
            projectPending = true
            return
        }
        projecting = true
        let doc = self.doc
        let pendingMessageIds = Set(pendingSends.map(\.messageId))
        let chatId = self.chatId
        let observedAt = lastRemoteUpdateAt
        let metadataOnly = self.metadataOnly
        Task { @MainActor [weak self] in
            let (decoded, failures) = await Task.detached(priority: .userInitiated) {
                if metadataOnly {
                    return (Optional(Self.decodeMetadata(from: doc, chatId: chatId)), [String: String]())
                }
                let root = doc.getDeepValue().mapValue
                return (root.map { Self.decodeProjection(from: $0, chatId: chatId, observedAt: observedAt) },
                        Self.commandFailures(from: root?["commands"]?.listValue ?? [],
                                             messageIds: pendingMessageIds))
            }.value
            guard let self else { return }
            self.projecting = false
            // An old detached projection may finish after the binding swap.
            // Never let it overwrite the recovered transcript or resolve echoes.
            guard self.doc === doc else {
                self.projectPending = false
                self.project()
                return
            }
            if let decoded {
                self.apply(decoded)
            }
            for (messageId, failure) in failures
                where self.pendingSends.contains(where: { $0.messageId == messageId }) {
                self.reportSendFailure(failure, messageId: messageId, terminal: true)
            }
            if self.projectPending {
                self.projectPending = false
                self.project()
            }
        }
    }

    private func apply(_ decoded: Projection) {
        entries = decoded.entries
        publishedSession = decoded.session
        transcriptActivity = decoded.activity
        publishedEnvironment = decoded.environment
        previewTitle = decoded.previewTitle
        // Drop echoes the host has materialized.
        let ids = Set(entries.map(\.id))
        pendingSends.removeAll { ids.contains($0.messageId) }
        for id in ids {
            if let draft = submittedDrafts.removeValue(forKey: id) {
                for image in draft.images { uploadedImages.removeValue(forKey: image.id) }
            }
        }
        revision &+= 1
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

    /// Whole-doc decode. `nil` means the doc has no map root yet — leave the
    /// previous projection standing rather than blanking a live transcript.
    nonisolated static func decodeEntries(from doc: LoroDoc) -> [MessageEntry]? {
        guard let root = doc.getDeepValue().mapValue else { return nil }
        return decodeEntries(from: root)
    }

    nonisolated private static func decodeEntries(from root: [String: LoroValue]) -> [MessageEntry] {
        let raw = (root["messages"]?.listValue ?? []).compactMap(entryFrom)
        return joinContinuations(raw)
    }

    private struct Projection {
        var entries: [MessageEntry]
        var session: SessionRow?
        var activity: SessionRow?
        var environment: SessionEnvironment?
        var previewTitle: String?
    }

    nonisolated private static func decodeProjection(
        from root: [String: LoroValue], chatId: String, observedAt: Int64?
    ) -> Projection {
        let raw = (root["messages"]?.listValue ?? []).compactMap(entryFrom)
        var session: SessionRow?
        var environment: SessionEnvironment?
        for publication in (root["publications"]?.listValue ?? []).reversed() {
            guard let record = publication.mapValue?["record"]?.mapValue,
                  record["kind"]?.stringValue == "agentSession",
                  let value = record["value"]?.mapValue,
                  value["chatId"]?.stringValue == chatId,
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
        return Projection(entries: joinContinuations(raw), session: session,
                          activity: activity(in: raw, chatId: chatId, observedAt: observedAt),
                          environment: environment, previewTitle: titlePreview(in: raw))
    }

    nonisolated private static func titlePreview(in entries: [MessageEntry]) -> String? {
        guard let first = entries.first(where: { $0.role == .user }) else { return nil }
        let text = first.parts.compactMap { part -> String? in
            if case .text(_, let text) = part { return text }
            return nil
        }.joined(separator: " ")
        guard let title = normalizedSessionTitle(text) else { return nil }
        return String(title.prefix(48)) + (title.count > 48 ? "…" : "")
    }

    nonisolated private static func decodeMetadata(from doc: LoroDoc, chatId: String) -> Projection {
        let publications = doc.getList(id: "publications")
        var latest: [LoroValue] = []
        for index in (0..<publications.len()).reversed() {
            guard let item = publications.get(index: index),
                  let value = item.asValue() ?? item.asLoroMap()?.getDeepValue(),
                  let record = value.mapValue?["record"]?.mapValue,
                  record["kind"]?.stringValue == "agentSession",
                  record["value"]?.mapValue?["chatId"]?.stringValue == chatId else { continue }
            latest = [value]
            break
        }
        var decoded = decodeProjection(from: ["publications": .list(value: latest)],
                                       chatId: chatId, observedAt: nil)
        let messages = doc.getList(id: "messages")
        for index in 0..<messages.len() {
            guard let item = messages.get(index: index) else { continue }
            // Check the scalar role before converting a possibly large tool row.
            let map = item.asLoroMap()
            let value = item.asValue()
            let role = value?.mapValue?["role"]?.stringValue ?? map?.get(key: "role")?.asValue()?.stringValue
            guard role == "user", let value = value ?? map?.getDeepValue(),
                  let entry = entryFrom(value) else { continue }
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
                            continuationOf: m["continuationOf"]?.stringValue)
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
            if let rootId = entry.continuationOf, let ix = index[rootId] {
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
