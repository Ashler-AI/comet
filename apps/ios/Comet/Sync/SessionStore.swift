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

    private(set) var doc = LoroDoc()
    private var room: RoomClient?
    private var subscriptions: [Subscription] = []
    @ObservationIgnored private var roomEpoch: UInt64 = 0
    private let config: AppConfig

    /// Demo mode: no room, entries driven externally.
    private let offline: Bool
    /// Demo hook: invoked instead of the command plane when offline.
    @ObservationIgnored var demoResponder: ((String) -> Void)?
    /// The trusted desktop host/controller admits every command. Transport and
    /// admission errors are handled here alongside the matching optimistic echo.
    @ObservationIgnored var commandSender: ((SessionCommandPayload) async throws -> Void)?

    init(chatId: String, config: AppConfig, deploymentId: String? = nil, offline: Bool = false) {
        self.chatId = chatId
        self.config = config
        self.deploymentId = deploymentId
        self.offline = offline
    }

    /// Demo-mode injection point (also used by previews).
    func setEntries(_ new: [MessageEntry]) {
        entries = new
        revision &+= 1
    }

    @ObservationIgnored private var saver: DocSaver?

    func start() {
        guard room == nil, !offline else { return }
        roomEpoch &+= 1
        let epoch = roomEpoch
        // Local-first: last-synced transcript renders instantly (even when the
        // host device is offline); the join backfills incrementally from here.
        if DocDisk.load(into: doc, id: chatId) {
            project()
        }
        saver = DocSaver(docId: chatId, doc: doc)
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
        Task { @MainActor [weak self] in
            let (decoded, failures) = await Task.detached(priority: .userInitiated) {
                let root = doc.getDeepValue().mapValue
                return (root.map { Self.decodeEntries(from: $0) },
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
                self.reportSendFailure(failure, messageId: messageId)
            }
            if self.projectPending {
                self.projectPending = false
                self.project()
            }
        }
    }

    private func apply(_ decoded: [MessageEntry]) {
        entries = decoded
        // Drop echoes the host has materialized.
        let ids = Set(entries.map(\.id))
        pendingSends.removeAll { ids.contains($0.messageId) }
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

    func sendRun(prompt: String, chat: Chat?) {
        if offline {
            demoResponder?(prompt)
            return
        }
        let messageId = UUID().uuidString.lowercased()
        let request = RunRequest(prompt: prompt,
                                 model: chat?.config?.model,
                                 reasoning: chat?.config?.reasoning,
                                 cwd: chat?.cwd ?? "",
                                 sandbox: chat?.config?.sandbox ?? "workspace-write")
        stagePendingSend(prompt: prompt, messageId: messageId)
        sendCommand(.run(request: request, messageId: messageId))
    }

    func sendSteer(prompt: String) {
        if offline {
            demoResponder?(prompt)
            return
        }
        let messageId = UUID().uuidString.lowercased()
        stagePendingSend(prompt: prompt, messageId: messageId)
        sendCommand(.steer(prompt: prompt, messageId: messageId))
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

    func reportSendFailure(_ message: String, messageId: String?) {
        failedPrompt = pendingSends.first(where: { $0.messageId == messageId })?.text
        if let messageId { dropPendingSend(messageId: messageId) }
        sendFailure = message
    }

    func clearSendFailure() {
        sendFailure = nil
        failedPrompt = nil
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
