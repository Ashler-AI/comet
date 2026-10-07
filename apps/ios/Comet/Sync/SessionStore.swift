// Session doc mirror — transcript entries for one chat (crates/doc/src/schema.rs).
// Commands go through the authenticated host RPC; the phone never writes the
// durable command ledger. Optimistic echoes keep their client-minted message
// ids until the host materializes them, or admission fails.

import Foundation
import Loro
import Observation
import UIKit

struct MobileCommandReceipt: Decodable {
    var commandId: String
    var metadataError: String?
    var preparationError: String?
}

struct MobileCommandAdmission: Codable {
    var commandId: String
    var issuedAt: Int64
    var expiresAt: Int64
    var hostDeviceId: String?
    var scaffold: ScaffoldAuthority?
    var scope: CollaborationScope?

    struct ScaffoldAuthority: Codable {
        var controllerDeviceId: String
        var ownerDeviceId: String
        var actorSubject: String
        var grantId: String
        var projection: SessionRoomProjection
        var environment: SessionEnvironment
        var preparationGeneration: String?
        init(_ route: ScaffoldControlRoute) {
            controllerDeviceId = route.controllerDeviceId; ownerDeviceId = route.ownerDeviceId
            actorSubject = route.actorSubject; grantId = route.grantId
            projection = route.projection; environment = route.environment
            preparationGeneration = route.preparationGeneration
        }
        var route: ScaffoldControlRoute {
            ScaffoldControlRoute(controllerDeviceId: controllerDeviceId, ownerDeviceId: ownerDeviceId,
                actorSubject: actorSubject, grantId: grantId, projection: projection,
                environment: environment, preparationGeneration: preparationGeneration)
        }
    }
}
@MainActor
@Observable
final class SessionStore {
    let chatId: String
    private(set) var deploymentId: String?
    private(set) var entries: [MessageEntry] = []
    private(set) var publishedSession: SessionRow?
    private(set) var publishedEnvironment: SessionEnvironment?
    private(set) var publishedHasActiveChildren = false
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
    private(set) var hasAuthoritativeProjection = false
    /// Client-minted ids of sends the host hasn't materialized yet.
    private(set) var pendingSends: [(messageId: String, text: String, at: Int64)] = []
    private(set) var sendFailure: String?
    private(set) var failedPrompt: String?
    private(set) var failedImages: [MobileImageAttachment] = []
    private(set) var sending = false
    @ObservationIgnored var attachmentUploader: (([MobileImageAttachment]) async throws -> [String])?
    @ObservationIgnored private var uploadedImages: [UUID: String] = [:]
    @ObservationIgnored private var submittedDrafts: [String: SubmittedDraft] = [:]
    @ObservationIgnored private var terminalControlOutcomes: [String: ControlOutcome] = [:]
    private struct ControlOutcome: Decodable {
        var messageId: String
        var prompt: String
        var terminal: Bool
        var failure: String?
        var control: SubmittedDraft.Control?
    }
    @ObservationIgnored private var retryDraft: SubmittedDraft?

    private struct SubmittedDraft: Codable {
        var messageId: String
        var prompt: String
        var images: [MobileImageAttachment]
        var steer: Bool
        var request: RunRequest?
        var prepared = false
        var createdAt = nowMs()
        var expiresAt = nowMs() + 86_400_000
        var failure: String?
        var terminal = false
        var admitted = false
        var admissionAttempted = false
        var admission: MobileCommandAdmission?
        var steerPrompt: String?
        enum Control: Codable {
            case interrupt
            case respondInput(requestId: String, answers: [UserInputAnswer])
        }
        var control: Control?
        var payload: SessionCommandPayload? {
            get {
                guard prepared else { return nil }
                if case .interrupt = control { return .interrupt }
                if case .respondInput(let id, let answers) = control { return .respondInput(requestId: id, answers: answers) }
                if let request { return .run(request: request, messageId: messageId) }
                return .steer(prompt: steerPrompt ?? prompt, messageId: messageId)
            }
            set {
                prepared = newValue != nil
                control = nil
                if case .run(let value, _) = newValue { request = value }
                else if case .steer(let value, _) = newValue { steerPrompt = value; request = nil }
                else if case .interrupt = newValue { control = .interrupt; request = nil }
                else if case .respondInput(let id, let answers) = newValue { control = .respondInput(requestId: id, answers: answers); request = nil }
                else { request = nil }
            }
        }
        private struct Image: Codable { var id: UUID; var filename: String; var bytes: Data }
        private enum CodingKeys: String, CodingKey {
            case messageId, prompt, images, steer, request, prepared, createdAt, expiresAt, failure, terminal, admitted, admissionAttempted, admission, steerPrompt, control
        }
        init(messageId: String, prompt: String, images: [MobileImageAttachment], steer: Bool) {
            self.messageId = messageId; self.prompt = prompt; self.images = images; self.steer = steer
            expiresAt = createdAt + 86_400_000
        }
        init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            messageId = try c.decode(String.self, forKey: .messageId)
            prompt = try c.decode(String.self, forKey: .prompt)
            steer = try c.decode(Bool.self, forKey: .steer)
            request = try c.decodeIfPresent(RunRequest.self, forKey: .request)
            prepared = try c.decode(Bool.self, forKey: .prepared)
            createdAt = try c.decode(Int64.self, forKey: .createdAt)
            expiresAt = try c.decode(Int64.self, forKey: .expiresAt)
            failure = try c.decodeIfPresent(String.self, forKey: .failure)
            terminal = try c.decode(Bool.self, forKey: .terminal)
            admitted = try c.decode(Bool.self, forKey: .admitted)
            admissionAttempted = try c.decodeIfPresent(Bool.self, forKey: .admissionAttempted) ?? prepared
            admission = try c.decodeIfPresent(MobileCommandAdmission.self, forKey: .admission)
            steerPrompt = try c.decodeIfPresent(String.self, forKey: .steerPrompt)
            control = try c.decodeIfPresent(Control.self, forKey: .control)
            let retainedImages = try c.decode([Image].self, forKey: .images)
            guard retainedImages.count <= MobileImageAttachment.maximumCount,
                  retainedImages.allSatisfy({ $0.bytes.count <= MobileImageAttachment.maximumImageBytes }) else {
                throw MobileSessionError.unavailable("The retained Crew images exceed their safe limits.")
            }
            images = try retainedImages.map {
                guard let preview = UIImage(data: $0.bytes) else {
                    throw MobileSessionError.unavailable("A retained Crew image cannot be decoded; its original is still on disk.")
                }
                return MobileImageAttachment(id: $0.id, filename: $0.filename, bytes: $0.bytes, preview: preview)
            }
        }
        func encode(to encoder: Encoder) throws {
            var c = encoder.container(keyedBy: CodingKeys.self)
            try c.encode(messageId, forKey: .messageId); try c.encode(prompt, forKey: .prompt)
            try c.encode(steer, forKey: .steer); try c.encodeIfPresent(request, forKey: .request)
            try c.encode(prepared, forKey: .prepared); try c.encode(createdAt, forKey: .createdAt)
            try c.encode(expiresAt, forKey: .expiresAt); try c.encodeIfPresent(failure, forKey: .failure)
            try c.encode(terminal, forKey: .terminal); try c.encode(admitted, forKey: .admitted)
            try c.encode(admissionAttempted, forKey: .admissionAttempted)
            try c.encodeIfPresent(admission, forKey: .admission)
            try c.encodeIfPresent(steerPrompt, forKey: .steerPrompt)
            try c.encodeIfPresent(control, forKey: .control)
            try c.encode(images.map { Image(id: $0.id, filename: $0.filename, bytes: $0.bytes) }, forKey: .images)
        }
    }

    private var intentCacheId: String { config.documentCacheId(roomId: chatId, deploymentId: deploymentId) }
    @ObservationIgnored private var legacyCacheId: String?
    @ObservationIgnored private var intentLoadBlocked = false
    private(set) var recoveryFailure: String?
    private(set) var composerText = ""
    private(set) var composerImages: [MobileImageAttachment] = []
    @ObservationIgnored private var composerSaveTask: Task<Void, Never>?

    func retainComposer(text: String, images: [MobileImageAttachment]) {
        composerText = text; composerImages = images
        if let retryDraft, retryDraft.admitted, retryDraft.terminal, retryDraft.failure == nil,
           text.trimmingCharacters(in: .whitespacesAndNewlines) != retryDraft.prompt { self.retryDraft = nil }
        composerSaveTask?.cancel()
        composerSaveTask = Task { @MainActor [weak self] in
            do { try await Task.sleep(nanoseconds: 300_000_000) } catch { return }
            guard let self else { return }
            do { try self.persistDrafts() } catch { self.sendFailure = error.localizedDescription }
            self.composerSaveTask = nil
        }
    }

    var deliveryStatus: String? {
        _ = revision
        if submittedDrafts.values.contains(where: { $0.admitted && !$0.terminal }) { return "Accepted · delivery pending" }
        if sending || !pendingSends.isEmpty { return "Awaiting Crew admission…" }
        if submittedDrafts.values.contains(where: { $0.admissionAttempted && !$0.terminal }) { return "Admission unknown · retry original instruction" }
        return nil
    }

    var retainedDrafts: [(id: String, prompt: String)] {
        _ = revision
        return submittedDrafts.values.filter { $0.control == nil && (!$0.terminal || $0.failure != nil) }.sorted { $0.createdAt < $1.createdAt }.map { ($0.messageId, $0.prompt) }
    }

    var retainedControls: [(id: String, prompt: String, terminal: Bool)] {
        _ = revision
        let pending = submittedDrafts.values.filter { $0.control != nil }.sorted { $0.createdAt < $1.createdAt }.map { ($0.messageId, $0.prompt, $0.terminal) }
        let terminal = terminalControlOutcomes.values.sorted { $0.messageId < $1.messageId }.map { ($0.messageId, $0.prompt, true) }
        return pending + terminal
    }

    func retryControl(_ id: String) {
        guard let draft = submittedDrafts[id], draft.control != nil, !draft.terminal else { return }
        Task { await submitControl(draft) }
    }

    func selectRetainedDraft(_ id: String) {
        guard let draft = submittedDrafts[id] else { return }
        retryDraft = draft
        failedPrompt = draft.prompt; failedImages = draft.images
        sendFailure = draft.failure ?? (draft.admitted ? "Crew accepted this instruction. Retry checks its original identity and pending delivery." : "Crew retained this instruction. Review before retrying.")
        revision &+= 1
    }

    // Count immutable attachments once across retained sends and the composer.
    // Reusing an identity for different bytes or metadata is never a retry.
    private static func validateAttachments(_ groups: [[MobileImageAttachment]]) throws {
        var unique: [UUID: MobileImageAttachment] = [:]
        var total = 0
        for images in groups {
            guard images.count <= MobileImageAttachment.maximumCount,
                  Set(images.map(\.id)).count == images.count else {
                throw MobileSessionError.unavailable("Crew retained too many or duplicate images in one instruction.")
            }
            for image in images {
                guard image.bytes.count <= MobileImageAttachment.maximumImageBytes else {
                    throw MobileSessionError.unavailable("Crew retained an image larger than 24 MB.")
                }
                if let original = unique[image.id] {
                    guard original.filename == image.filename, original.bytes == image.bytes else {
                        throw MobileSessionError.unavailable("Crew retained conflicting payloads for one attachment identity.")
                    }
                } else {
                    guard image.bytes.count <= MobileImageAttachment.maximumTotalBytes - total else {
                        throw MobileSessionError.unavailable("Crew retains too many unacknowledged images. Resolve those sends before attaching more.")
                    }
                    total += image.bytes.count
                    unique[image.id] = image
                }
            }
        }
    }

    private func validateDrafts(_ drafts: [SubmittedDraft]) throws {
        guard drafts.count <= 17,
              Set(drafts.map(\.messageId)).count == drafts.count,
              drafts.allSatisfy({ $0.admission?.scope == nil || ($0.admission?.scope?.projectId == config.projectScope && $0.admission?.scope?.deploymentId == deploymentId && $0.admission?.scope?.sessionId == chatId) }),
              drafts.allSatisfy({ $0.messageId == "composer" || UUID(uuidString: $0.messageId) != nil }),
              drafts.allSatisfy({ $0.admission == nil || ($0.admission?.commandId == $0.messageId && $0.admission?.issuedAt == $0.createdAt && $0.admission?.expiresAt == $0.expiresAt) }),
              drafts.allSatisfy({ let lifetime = $0.expiresAt.subtractingReportingOverflow($0.createdAt); return !lifetime.overflow && lifetime.partialValue > 0 && lifetime.partialValue <= 86_400_000 }) else {
            throw MobileSessionError.unavailable("The retained Crew journal has conflicting or oversized records.")
        }
        try Self.validateAttachments(drafts.map(\.images))
    }

    private func persistDrafts() throws {
        guard !offline, !metadataOnly, AppConfig.canonicalSessionId(chatId) != nil else { return }
        guard !intentLoadBlocked else { throw MobileSessionError.unavailable("Crew draft recovery is blocked; the original journal is retained.") }
        var drafts = submittedDrafts
        if let retryDraft, !(retryDraft.admitted && retryDraft.terminal && retryDraft.failure == nil) { drafts[retryDraft.messageId] = retryDraft }
        try Self.validateAttachments(drafts.values.map(\.images) + [composerImages])
        let representedComposer = drafts.values.contains { $0.control == nil && $0.prompt == composerText.trimmingCharacters(in: .whitespacesAndNewlines) && $0.images.map(\.id) == composerImages.map(\.id) }
        if (!composerText.isEmpty || !composerImages.isEmpty), !representedComposer {
            drafts["composer"] = SubmittedDraft(messageId: "composer", prompt: composerText, images: composerImages, steer: false)
        }
        let retained = Array(drafts.values)
        try validateDrafts(retained)
        try DocDisk.saveIntents(retained, id: intentCacheId)
    }

    private func restoreDrafts() {
        guard !offline, !metadataOnly, AppConfig.canonicalSessionId(chatId) != nil else { return }
        do {
            var loaded = try DocDisk.loadIntents([SubmittedDraft].self, id: intentCacheId)
            if loaded == nil, let legacyCacheId {
                loaded = try DocDisk.loadIntents([SubmittedDraft].self, id: legacyCacheId)
                if let loaded { try validateDrafts(loaded); try DocDisk.saveIntents(loaded, id: intentCacheId) }
            }
            let drafts = loaded ?? []
            try validateDrafts(drafts)
            terminalControlOutcomes.removeAll()
            let prefix = DocDisk.intentURL(for: intentCacheId).lastPathComponent + "."
            var completedIds: Set<String> = []
            for url in try FileManager.default.contentsOfDirectory(at: DocDisk.directory, includingPropertiesForKeys: nil)
                where url.lastPathComponent.hasPrefix(prefix) && url.pathExtension == "outcome" {
                let attributes = try FileManager.default.attributesOfItem(atPath: url.path)
                guard let size = attributes[.size] as? NSNumber, size.int64Value <= 64 * 1024 * 1024 else {
                    throw MobileSessionError.unavailable("A retained Crew outcome exceeds its safe size limit.")
                }
                let outcome = try JSONDecoder().decode(ControlOutcome.self, from: Data(contentsOf: url))
                if outcome.terminal, outcome.control != nil, outcome.failure != nil { terminalControlOutcomes[outcome.messageId] = outcome }
                if outcome.terminal, outcome.failure == nil { completedIds.insert(outcome.messageId) }
            }
            if let composer = drafts.first(where: { $0.messageId == "composer" }) {
                composerText = composer.prompt; composerImages = composer.images
            }
            // Migrate terminal controls only after retaining their durable outcome.
            for draft in drafts where draft.terminal && draft.control != nil {
                try DocDisk.retainOutcome(draft, id: intentCacheId, commandId: draft.messageId)
                if draft.failure != nil {
                    terminalControlOutcomes[draft.messageId] = ControlOutcome(messageId: draft.messageId, prompt: draft.prompt, terminal: true, failure: draft.failure, control: draft.control)
                }
            }
            let sends = drafts.filter { $0.messageId != "composer" && !($0.terminal && $0.control != nil) && !completedIds.contains($0.messageId) }
            submittedDrafts = Dictionary(uniqueKeysWithValues: sends.map { ($0.messageId, $0) })
            if let composer = drafts.first(where: { $0.messageId == "composer" }),
               drafts.contains(where: { completedIds.contains($0.messageId) && $0.prompt == composer.prompt && $0.images.map(\.id) == composer.images.map(\.id) }) {
                composerText = ""; composerImages = []
            }
            if sends.count != drafts.filter({ $0.messageId != "composer" }).count { try persistDrafts() }
            // A reply may have been lost; don't lock the composer behind an
            // uncertain echo. Deliberate retry still uses the original ID.
            pendingSends = []
            if let draft = sends.filter({ $0.control == nil && !$0.admitted }).sorted(by: { $0.createdAt < $1.createdAt }).last {
                retryDraft = draft
                failedPrompt = draft.prompt; failedImages = draft.images
                sendFailure = draft.failure ?? "Crew retained this send. Retry checks its original identity; it is not automatically sent."
            }
            revision &+= 1
        } catch {
            intentLoadBlocked = true
            recoveryFailure = "Crew cannot recover retained drafts: \(error.localizedDescription)"
            sendFailure = recoveryFailure
        }
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
    @ObservationIgnored var commandSender: ((SessionCommandPayload, MobileCommandAdmission) async throws -> Void)?
    @ObservationIgnored var commandReader: ((MobileCommandAdmission) async throws -> [String: Any])?
    @ObservationIgnored var commandHostDeviceId: String?
    @ObservationIgnored var commandHostProvider: (() -> String?)?
    @ObservationIgnored var commandRouteProvider: (() async throws -> ScaffoldControlRoute?)?
    @ObservationIgnored var commandScaffoldRoute: ScaffoldControlRoute?

    init(chatId: String, config: AppConfig, deploymentId: String? = nil, offline: Bool = false,
         metadataOnly: Bool = false) {
        self.chatId = AppConfig.canonicalSessionId(chatId) ?? chatId
        self.config = config
        self.deploymentId = deploymentId
        self.offline = offline
        self.metadataOnly = metadataOnly
        if self.chatId != chatId {
            legacyCacheId = config.documentCacheId(roomId: chatId, deploymentId: deploymentId)
        }
        restoreDrafts()
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
        restoreDrafts()
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
        let legacyCacheId = self.legacyCacheId
        let load = Task.detached(priority: .userInitiated) {
            DocDisk.loadReplica(id: cacheId) ?? legacyCacheId.flatMap { DocDisk.loadReplica(id: $0) }
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
        guard AppConfig.canonicalSessionId(chatId) != nil else {
            recoveryFailure = "This Crew record has no canonical public session identity. Its local records are retained; it cannot be opened as a room."
            project()
            return
        }
        let client = RoomClient(roomId: chatId, doc: doc, readOnly: true) { [config, chatId, deploymentId] in
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
        guard doc === previous, let room else { return false }
        // The phone is read-only in session rooms. Its sends live in the scoped
        // journal, not in Loro ancestry. Retain old/legacy command evidence,
        // but never revive an instruction missing from the authoritative room.
        do {
            try DocDisk.retainRecoveryOriginal(doc: previous, id: intentCacheId)
            try saver?.replaceDocument(with: replacement)
        } catch { recoveryFailure = "Crew could not retain recovery records: \(error.localizedDescription)"; return false }
        // Keep entries visible until the replacement projection is ready, and
        // retain optimistic sends, command admission, and the reveal state.
        invalidateProjection()
        subscriptions.removeAll()
        doc = replacement
        subscribeLocalUpdates(client: room)
        project()
        return true
    }

    /// Backgrounding hook: persist immediately.
    func flushToDisk() {
        saver?.flush()
        do { try persistDrafts() } catch { sendFailure = error.localizedDescription }
        composerSaveTask?.cancel(); composerSaveTask = nil
    }

    func probeSync() async { await room?.probe(force: true) }
    func retryRecovery() async { recoveryFailure = nil; await room?.start() }

    func stop() {
        roomEpoch &+= 1
        composerSaveTask?.cancel(); composerSaveTask = nil
        do { try persistDrafts() } catch { sendFailure = error.localizedDescription }
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
        guard !sending else {
            recoveryFailure = "Crew cannot switch deployment while a send is in flight."
            return
        }
        do { try persistDrafts() } catch { recoveryFailure = error.localizedDescription; return }
        if !offline { stop() }
        legacyCacheId = nil
        deploymentId = value
        guard !offline else { return }
        doc = LoroDoc()
        hydrationComplete = false
        hasAuthoritativeProjection = false
        if !entries.isEmpty {
            entries = []
            revision &+= 1
        }
        publishedSession = nil
        publishedEnvironment = nil
        publishedHasActiveChildren = false
        previewTitle = nil
        transcriptActivity = nil
        lastRemoteUpdateAt = nil
        hasRevealed = false
        transcriptBuilder.reset()
        composerText = ""; composerImages = []
        uploadedImages.removeAll()
        submittedDrafts.removeAll(); retryDraft = nil; pendingSends.removeAll()
        clearSendFailure(); intentLoadBlocked = false
        restoreDrafts()
        start()
    }

    private func handle(_ event: RoomEvent) {
        switch event {
        case .connected:
            connected = true
            recoveryFailure = nil
            project()
        case .disconnected:
            connected = false
        case .remoteUpdate:
            lastRemoteUpdateAt = nowMs()
            project()
            saver?.poke()
        case .ephemeralUpdate, .localChangesAcknowledged:
            break
        case .recoveryBlocked(let message):
            connected = false
            recoveryFailure = message
        }
    }
    @ObservationIgnored private var publicationCache: PublicationCache?

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
        var completions: Set<String> = []
    }

    private func invalidateProjection() {
        publicationCache = nil
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
        let pendingMessageIds = Set(pendingSends.map(\.messageId)).union(submittedDrafts.values.filter { !$0.terminal }.map(\.messageId))
        let chatId = self.chatId
        let observedAt = lastRemoteUpdateAt
        let metadataOnly = self.metadataOnly
        let previousKey = lastProjectionKey
        let previousEntries = entries
        let publicationCache = self.publicationCache
        let previousActivity = transcriptActivity
        let work = Task.detached(priority: .userInitiated) { () -> ProjectionResult? in
            guard !Task.isCancelled else { return nil }
            let key = ProjectionKey(version: doc.stateVv(), metadataOnly: metadataOnly,
                                    observedAt: metadataOnly ? nil : observedAt,
                                    pendingMessageIds: pendingMessageIds)
            if key == previousKey {
                return ProjectionResult(key: key, decoded: nil, entriesChanged: false, failures: [:])
            }
            var decoded = metadataOnly
                ? Self.decodeMetadata(from: doc, chatId: chatId, publicationCache: publicationCache)
                : Self.decodeProjection(from: doc, chatId: chatId, observedAt: observedAt, publicationCache: publicationCache)
            let entriesChanged = !metadataOnly && decoded.entries != previousEntries
            if !entriesChanged, var activity = decoded.activity, activity.status == .working {
                // A status heartbeat is not transcript activity. Keep the last
                // actual content timestamp so a stale stream becomes unreachable.
                activity.updatedAt = previousActivity?.updatedAt ?? activity.startedAt ?? activity.updatedAt
                decoded.activity = activity
            }
            guard !Task.isCancelled else { return nil }
            let commands = pendingMessageIds.isEmpty || metadataOnly ? [] : doc.getList(id: "commands").getDeepValue().listValue ?? []
            let failures = Self.commandFailures(from: commands, messageIds: pendingMessageIds)
            let completions = Set(commands.compactMap { value -> String? in
                guard let command = value.mapValue, command["status"]?.stringValue == "applied",
                      let id = command["id"]?.stringValue, pendingMessageIds.contains(id) else { return nil }
                return id
            })
            return ProjectionResult(key: key, decoded: decoded,
                                    entriesChanged: entriesChanged,
                                    failures: failures, completions: completions)
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
            for (messageId, failure) in result.failures
                where self.submittedDrafts[messageId] != nil || self.pendingSends.contains(where: { $0.messageId == messageId }) {
                self.reportSendFailure(failure, messageId: messageId, terminal: true)
            }
            if let decoded = result.decoded {
                self.projectionCount &+= 1
                self.apply(decoded, metadataOnly: metadataOnly, entriesChanged: result.entriesChanged)
            }
            if self.connected, !doc.isDetached(), doc.stateVv() == doc.oplogVv() {
                self.hasAuthoritativeProjection = true
            }
            for id in result.completions { self.resolveAcceptedDraft(id) }
            if !result.completions.isEmpty {
                do { try self.persistDrafts() } catch { self.sendFailure = error.localizedDescription }
            }
            if self.projectPending {
                self.projectPending = false
                self.project()
            }
        }
    }

    private func apply(_ decoded: Projection, metadataOnly: Bool, entriesChanged: Bool) {
        publicationCache = decoded.publicationCache
        let validPublication = decoded.environment.map {
            $0.scope.projectId == config.projectScope && $0.scope.deploymentId == deploymentId &&
            (AppConfig.canonicalSessionId($0.scope.sessionId ?? "") ?? $0.scope.sessionId) == chatId
        } ?? true
        let session = validPublication ? decoded.session : nil
        let environment = validPublication ? decoded.environment : nil
        if publishedSession != session { publishedSession = session }
        if publishedEnvironment != environment { publishedEnvironment = environment }
        if !validPublication { recoveryFailure = "Crew ignored an owner publication for a different project, deployment, or session. Original records are retained." }
        publishedHasActiveChildren = validPublication && decoded.publicationHasActiveChildren
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
                resolveAcceptedDraft(id)
            }
        }
        if let retryDraft, retryDraft.failure == nil, entries.contains(where: { $0.id == retryDraft.messageId }) {
            self.retryDraft?.admitted = true
            self.retryDraft?.terminal = true
            clearSendFailure()
        }
        do { try persistDrafts() } catch { sendFailure = error.localizedDescription }
        if entriesChanged || pendingSends.count != pendingCount { revision &+= 1 }
    }

    private func resolveAcceptedDraft(_ id: String) {
        guard var draft = submittedDrafts[id], !(draft.terminal && draft.failure != nil) else { return }
        draft.admitted = true; draft.terminal = true; draft.failure = nil
        do {
            try DocDisk.retainOutcome(draft, id: intentCacheId, commandId: id)
            submittedDrafts.removeValue(forKey: id)
            dropPendingSend(messageId: id)
            for image in draft.images { uploadedImages.removeValue(forKey: image.id) }
            if retryDraft?.messageId == id { retryDraft = draft }
            if draft.control == nil, composerText.trimmingCharacters(in: .whitespacesAndNewlines) == draft.prompt,
               composerImages.map(\.id) == draft.images.map(\.id) { composerText = ""; composerImages = [] }
            revision &+= 1
        } catch { recoveryFailure = "Crew could not retain the accepted instruction outcome: \(error.localizedDescription)" }
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
                ?? command["id"]?.stringValue
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
        var publicationCache: PublicationCache?
        var publicationHasActiveChildren = false
    }

    struct PublicationCache {
        var count: UInt32 = 0
        var anchors: [String: [String: LoroValue]] = [:]
    }

    nonisolated static func decodeProjection(
        from doc: LoroDoc, chatId: String, observedAt: Int64?, publicationCache: PublicationCache? = nil
    ) -> Projection {
        let raw = (doc.getList(id: "messages").getDeepValue().listValue ?? []).compactMap(entryFrom)
        var decoded = decodePublication(from: doc, chatId: chatId, publicationCache: publicationCache)
        decoded.entries = joinContinuations(raw)
        decoded.activity = activity(in: raw, chatId: chatId, observedAt: observedAt)
        decoded.previewTitle = titlePreview(in: raw)
        return decoded
    }

    nonisolated private static func decodePublication(from doc: LoroDoc, chatId: String,
                                                     publicationCache: PublicationCache? = nil) -> Projection {
        func payload(_ record: [String: LoroValue]) -> [String: LoroValue]? {
            guard record["kind"]?.stringValue == "agentSession", let value = record["value"]?.mapValue,
                  let subject = value["ownerSubject"]?.stringValue, !subject.isEmpty,
                  record["publishedBy"]?.stringValue == subject,
                  let publicId = value["chatId"]?.stringValue,
                  (AppConfig.canonicalSessionId(publicId) ?? publicId) == chatId,
                  value["ownerDeviceId"]?.stringValue != nil, value["createdAt"]?.i64Value != nil else { return nil }
            return value
        }
        func sameGeneration(_ value: [String: LoroValue], _ anchor: [String: LoroValue]) -> Bool {
            let environment = value["environment"]?.mapValue
            let original = anchor["environment"]?.mapValue
            return value["ownerSubject"] == anchor["ownerSubject"] && value["ownerDeviceId"] == anchor["ownerDeviceId"] &&
                value["source"] == anchor["source"] && environment?["ownerPrincipal"] == original?["ownerPrincipal"] &&
                environment?["scope"]?.mapValue?["projectId"] == original?["scope"]?.mapValue?["projectId"] &&
                environment?["scope"]?.mapValue?["deploymentId"] == original?["scope"]?.mapValue?["deploymentId"] &&
                environment?["source"]?.mapValue?["sandbox_id"] == original?["source"]?.mapValue?["sandbox_id"] &&
                environment?["source"]?.mapValue?["lifecycle_epoch"] == original?["source"]?.mapValue?["lifecycle_epoch"]
        }
        func newerAnchor(_ current: [String: LoroValue], _ next: [String: LoroValue]) -> Bool {
            guard current["sessionId"] == next["sessionId"], current["chatId"] == next["chatId"],
                  current["ownerSubject"] == next["ownerSubject"] else { return false }
            if current["ownerDeviceId"] == next["ownerDeviceId"], current["source"] == next["source"],
               current["environment"] == next["environment"] {
                let currentAt = current["updatedAt"]?.i64Value ?? current["createdAt"]?.i64Value ?? 0
                let nextAt = next["updatedAt"]?.i64Value ?? next["createdAt"]?.i64Value ?? 0
                func rank(_ value: [String: LoroValue]) -> Int {
                    switch value["status"]?.stringValue {
                    case "errored": return 2
                    case "idle": return 1
                    case "working", "awaitingInput": return 0
                    default: return -1
                    }
                }
                return nextAt > currentAt || (nextAt == currentAt && rank(next) >= rank(current))
            }
            func scaffoldIdentity(_ value: [String: LoroValue]) -> (sandbox: String, epoch: UInt64)? {
                guard value["source"]?.stringValue == "scaffold",
                      let device = value["ownerDeviceId"]?.stringValue, device.hasPrefix("comet-scaffold-"),
                      let separator = device.range(of: "-e", options: .backwards),
                      let epoch = UInt64(device[separator.upperBound...]), epoch > 0 else { return nil }
                let start = device.index(device.startIndex, offsetBy: "comet-scaffold-".count)
                guard separator.lowerBound >= start else { return nil }
                let sandbox = String(device[start..<separator.lowerBound])
                return sandbox.isEmpty ? nil : (sandbox, epoch)
            }
            guard let old = scaffoldIdentity(current), let new = scaffoldIdentity(next),
                  old.sandbox == new.sandbox, new.epoch > old.epoch else { return false }
            func validEnvironment(_ value: [String: LoroValue], epoch: UInt64) -> Bool {
                guard let environment = value["environment"]?.mapValue else { return true }
                let source = environment["source"]?.mapValue
                let advertisedEpoch = source?["lifecycle_epoch"]
                return environment["ownerPrincipal"] == value["ownerSubject"] && source?["kind"]?.stringValue == "scaffold" &&
                    source?["sandbox_id"]?.stringValue == old.sandbox &&
                    (advertisedEpoch == nil || advertisedEpoch == .null || advertisedEpoch?.i64Value.flatMap({ UInt64(exactly: $0) }) == epoch)
            }
            guard validEnvironment(current, epoch: old.epoch), validEnvironment(next, epoch: new.epoch) else { return false }
            guard let a = current["environment"]?.mapValue else { return true }
            guard let b = next["environment"]?.mapValue else { return false }
            return a["scope"] == b["scope"] && a["databaseEnvironment"] == b["databaseEnvironment"]
        }
        func collect(_ value: [String: LoroValue], writer: String, into cache: inout PublicationCache) {
            if let current = cache.anchors[writer], !newerAnchor(current, value) { return }
            cache.anchors[writer] = value
        }
        let publications = doc.getList(id: "publications")
        var cache = publicationCache ?? PublicationCache()
        if cache.count > publications.len() { cache = PublicationCache() }
        for index in cache.count..<publications.len() {
            guard let item = publications.get(index: index),
                  let record = (item.asValue() ?? item.asLoroMap()?.getDeepValue())?.mapValue?["record"]?.mapValue,
                  let value = payload(record),
                  let writer = AppConfig.canonicalSessionId(value["sessionId"]?.stringValue ?? chatId) else { continue }
            collect(value, writer: writer, into: &cache)
        }
        cache.count = publications.len()
        let registers = doc.getMap(id: "agentSessions").getDeepValue().mapValue ?? [:]
        if !registers.isEmpty {
            let needed = Set(registers.keys.filter { AppConfig.canonicalSessionId($0) == $0 }).union([chatId])
            if cache.anchors.keys.contains(where: { !needed.contains($0) }) {
                cache.anchors = cache.anchors.filter { needed.contains($0.key) }
            }
            // A register can arrive just after its immutable creation record.
            // Recover only missing anchors, not the entire audit on each pulse.
            for writer in needed where cache.anchors[writer] == nil {
                for index in 0..<publications.len() {
                    guard let item = publications.get(index: index),
                          let record = (item.asValue() ?? item.asLoroMap()?.getDeepValue())?.mapValue?["record"]?.mapValue,
                          let value = payload(record),
                          AppConfig.canonicalSessionId(value["sessionId"]?.stringValue ?? chatId) == writer else { continue }
                    collect(value, writer: writer, into: &cache)
                }
            }
        }
        guard let canonical = cache.anchors[chatId] else {
            return Projection(entries: [], session: nil, activity: nil, environment: nil, previewTitle: nil, publicationCache: cache)
        }
        var records = registers.isEmpty ? cache.anchors : [chatId: canonical]
        for (key, raw) in registers {
            guard AppConfig.canonicalSessionId(key) == key, let record = raw.mapValue,
                  let value = payload(record), AppConfig.canonicalSessionId(value["sessionId"]?.stringValue ?? "") == key,
                  let anchor = cache.anchors[key], sameGeneration(value, anchor), sameGeneration(value, canonical),
                  let updatedAt = value["updatedAt"]?.i64Value,
                  updatedAt >= (anchor["updatedAt"]?.i64Value ?? anchor["createdAt"]?.i64Value ?? 0) else { continue }
            records[key] = value
        }
        let owner = records[chatId] ?? canonical
        var awaiting: SessionRow?
        var working: SessionRow?
        var stale: SessionRow?
        var terminal: SessionRow?
        var hasActiveChildren = false
        let now = nowMs()
        for (writer, value) in records {
            guard sameGeneration(value, owner), let deviceId = value["ownerDeviceId"]?.stringValue,
                  let status = value["status"]?.stringValue.flatMap(SessionStatus.init(rawValue:)) else { continue }
            let updatedAt = value["updatedAt"]?.i64Value ?? value["createdAt"]?.i64Value ?? 0
            let row = SessionRow(chatId: chatId, deviceId: deviceId, status: status, startedAt: updatedAt, updatedAt: updatedAt)
            if status == .working || status == .awaitingInput {
                if writer != chatId { hasActiveChildren = true }
                if effectiveStatus(row, now: now) == nil {
                    if stale == nil || updatedAt > (stale?.updatedAt ?? Int64.min) { stale = row }
                } else if status == .awaitingInput {
                    if awaiting == nil || updatedAt > (awaiting?.updatedAt ?? Int64.min) { awaiting = row }
                } else if working == nil || updatedAt > (working?.updatedAt ?? Int64.min) { working = row }
            } else if terminal == nil || updatedAt > (terminal?.updatedAt ?? Int64.min) { terminal = row }
        }
        let chosen = awaiting ?? working ?? stale ?? terminal
        var environment: SessionEnvironment?
        if let value = owner["environment"], value.mapValue != nil,
           let bytes = try? JSONSerialization.data(withJSONObject: value.jsonObject) {
            environment = try? JSONDecoder().decode(SessionEnvironment.self, from: bytes)
        }
        return Projection(entries: [], session: chosen, activity: nil, environment: environment, previewTitle: nil,
                          publicationCache: cache, publicationHasActiveChildren: hasActiveChildren)
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

    nonisolated private static func decodeMetadata(from doc: LoroDoc, chatId: String,
                                                  publicationCache: PublicationCache? = nil) -> Projection {
        var decoded = decodePublication(from: doc, chatId: chatId, publicationCache: publicationCache)
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
                else if let bytes = try? JSONSerialization.data(withJSONObject: v.jsonObject, options: [.sortedKeys, .fragmentsAllowed]),
                        let json = String(data: bytes, encoding: .utf8) { fields[k] = json }
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

    func recordAdmissionReceipt(_ receipt: MobileCommandReceipt, admission: MobileCommandAdmission) throws {
        guard receipt.commandId == admission.commandId else {
            throw MobileSessionError.unavailable("Crew admission response did not identify the original command; its outcome is unknown.")
        }
        if var draft = submittedDrafts[receipt.commandId], !draft.terminal {
            guard let original = draft.admission,
                  NSDictionary(dictionary: encodableDictionary(original)).isEqual(to: encodableDictionary(admission)) else {
                throw MobileSessionError.unavailable("Crew admission receipt belongs to different retained authority; the original instruction is retained.")
            }
            draft.admitted = true; draft.failure = nil
            submittedDrafts[receipt.commandId] = draft
            do { try persistDrafts() }
            catch { recoveryFailure = "Crew accepted this instruction, but could not checkpoint its receipt: \(error.localizedDescription)" }
        }
        if let failure = receipt.metadataError ?? receipt.preparationError {
            recoveryFailure = "Crew accepted the instruction; metadata recovery needs attention: \(failure)"
        }
        revision &+= 1
    }

    @discardableResult
    func sendRun(prompt: String, chat: Chat?, images: [MobileImageAttachment] = []) async -> Bool {
        await sendMessage(prompt: prompt, chat: chat, images: images, steer: false)
    }

    @discardableResult
    func sendSteer(prompt: String, images: [MobileImageAttachment] = []) async -> Bool {
        await sendMessage(prompt: prompt, chat: nil, images: images, steer: true)
    }

    private func sendMessage(prompt: String, chat: Chat?, images: [MobileImageAttachment], steer: Bool) async -> Bool {
        guard offline || AppConfig.canonicalSessionId(chatId) != nil else {
            sendFailure = "This retained Crew record has no canonical public session identity; no instruction was sent."
            return false
        }
        guard !sending, !intentLoadBlocked, !prompt.isEmpty || !images.isEmpty else { return false }
        guard submittedDrafts.count < 16 || submittedDrafts.values.contains(where: { $0.prompt == prompt }) else {
            sendFailure = "Crew retains 16 unresolved sends. Review them before submitting another instruction."
            return false
        }
        do { try Self.validateAttachments(submittedDrafts.values.map(\.images) + [images]) }
        catch { sendFailure = error.localizedDescription; return false }
        sending = true
        defer { sending = false }
        clearSendFailure()
        let retry = retryDraft.flatMap {
            $0.prompt == prompt && $0.images.map(\.id) == images.map(\.id) ? $0 : nil
        } ?? submittedDrafts.values.first(where: { $0.control == nil && $0.prompt == prompt && $0.images.map(\.id) == images.map(\.id) })
        if let retry, !(retry.terminal && retry.failure != nil),
           (retry.admitted && retry.terminal || entries.contains(where: { $0.id == retry.messageId })) {
            resolveAcceptedDraft(retry.messageId)
            retryDraft = nil
            do { try persistDrafts() }
            catch { recoveryFailure = "Crew accepted this instruction, but could not checkpoint its outcome: \(error.localizedDescription)" }
            return true
        }
        var draft = retry ?? SubmittedDraft(messageId: UUID().uuidString.lowercased(), prompt: prompt,
                                            images: images, steer: steer)
        if let retry, retry.terminal {
            draft.messageId = UUID().uuidString.lowercased()
            draft.payload = nil
            draft.steer = steer
            draft.createdAt = nowMs(); draft.expiresAt = draft.createdAt + 86_400_000
            draft.failure = nil; draft.terminal = false; draft.admitted = false; draft.admissionAttempted = false; draft.admission = nil
            submittedDrafts.removeValue(forKey: retry.messageId)
        }
        draft.failure = nil
        submittedDrafts[draft.messageId] = draft
        if draft.admission == nil {
            draft.admission = MobileCommandAdmission(commandId: draft.messageId,
                issuedAt: draft.createdAt, expiresAt: draft.expiresAt,
                hostDeviceId: commandHostProvider?() ?? commandHostDeviceId,
                scaffold: commandScaffoldRoute.map(MobileCommandAdmission.ScaffoldAuthority.init))
            draft.admission?.scope = CollaborationScope(projectId: config.projectScope, deploymentId: deploymentId, sessionId: chatId)
            submittedDrafts[draft.messageId] = draft
        }
        // Keep this send's preparation receipt even if navigation/re-attach
        // reconfigures the store's transport while attachments are uploading.
        let attachmentUploader = attachmentUploader
        let commandSender = commandSender
        do {
            if retry != nil, let accepted = await readAdmission(&draft, retryPending: true) { return accepted }
            guard draft.expiresAt > nowMs() else {
                reportSendFailure(draft.admissionAttempted ? "This original Crew instruction's retry window expired; its admission outcome is still unknown. Check the original host before sending a new instruction." : "This retained Crew send expired before admission. Review it before sending a new instruction.", messageId: draft.messageId, terminal: !draft.admissionAttempted)
                return false
            }
            // Write before uploads or admission: a process death cannot erase a
            // user send or change its ID on an uncertain-response retry.
            try persistDrafts()
            if !draft.admissionAttempted, draft.admission?.scaffold == nil, let route = try await commandRouteProvider?() {
                draft.admission?.scaffold = MobileCommandAdmission.ScaffoldAuthority(route)
                submittedDrafts[draft.messageId] = draft
                try persistDrafts()
            }
            let missing = draft.prepared ? [] : images.filter { uploadedImages[$0.id] == nil }
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
            retryDraft = nil
            try persistDrafts()
            stagePendingSend(prompt: content, messageId: draft.messageId)
            guard let admission = draft.admission, admission.expiresAt > nowMs() else {
                throw MobileSessionError.unavailable("This Crew instruction has expired.")
            }
            draft.admissionAttempted = true
            submittedDrafts[draft.messageId] = draft
            try persistDrafts()
            try await commandSender(payload, admission)
            // Projection may report a rejection while admission is suspended.
            guard retryDraft?.messageId != draft.messageId || sendFailure == nil else { return false }
            retryDraft = nil
            if var accepted = submittedDrafts[draft.messageId] {
                accepted.admitted = true
                submittedDrafts[draft.messageId] = accepted
            }
            try persistDrafts()
            return true
        } catch {
            // A lost RPC response must not turn an already materialized send
            // into a second user message on deliberate retry.
            if entries.contains(where: { $0.id == draft.messageId }), submittedDrafts[draft.messageId]?.terminal != true { return true }
            if submittedDrafts[draft.messageId] == nil, retryDraft?.messageId == draft.messageId,
               retryDraft?.admitted == true { return true }
            if retryDraft?.messageId == draft.messageId, sendFailure != nil { return false }
            if let accepted = await readAdmission(&draft) { return accepted }
            if submittedDrafts[draft.messageId]?.admitted == true { return true }
            if let relay = error as? RelayError, case .rpc(let detail) = relay,
               ["command_id_conflict", "peer_command_scope_denied"].contains(where: { detail.contains($0) }) {
                reportSendFailure("Crew rejected this admission attempt: \(detail). The original instruction and identity are retained.", messageId: draft.messageId)
                return false
            }
            let message = draft.admissionAttempted
                ? "Crew admission outcome is unknown. Retry checks the original instruction: \(error.localizedDescription)"
                : error is CancellationError ? "Send cancelled before admission. Your draft is still here." : "Crew did not send this instruction: \(error.localizedDescription)"
            reportSendFailure(message, messageId: draft.messageId)
            return false
        }
    }

    @discardableResult
    func stagePendingSend(prompt: String, messageId: String = UUID().uuidString.lowercased()) -> String {
        guard !pendingSends.contains(where: { $0.messageId == messageId }) else { return messageId }
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
        if let messageId, var draft = submittedDrafts[messageId] {
            guard !draft.terminal || terminal else { return }
            draft.failure = message
            draft.terminal = terminal
            submittedDrafts[messageId] = draft
            if terminal {
                do {
                    try DocDisk.retainOutcome(draft, id: intentCacheId, commandId: messageId)
                    if draft.control != nil {
                        terminalControlOutcomes[messageId] = ControlOutcome(messageId: messageId, prompt: draft.prompt, terminal: true, failure: draft.failure, control: draft.control)
                        submittedDrafts.removeValue(forKey: messageId)
                    }
                } catch { recoveryFailure = "Crew could not retain the terminal outcome: \(error.localizedDescription)" }
            }
            revision &+= 1
            if draft.control == nil {
                retryDraft = draft
                failedPrompt = draft.prompt
                failedImages = draft.images
            } else { failedPrompt = nil; failedImages = [] }
        } else {
            failedPrompt = pendingSends.first(where: { $0.messageId == messageId })?.text
        }
        if let messageId { dropPendingSend(messageId: messageId) }
        sendFailure = message
        do { try persistDrafts() } catch { sendFailure = "\(message) Draft persistence failed: \(error.localizedDescription)" }
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
        var draft = SubmittedDraft(messageId: UUID().uuidString.lowercased(),
            prompt: payload.kind == "interrupt" ? "Stop Crew session" : "Respond to Crew input", images: [], steer: false)
        draft.expiresAt = draft.createdAt + 300_000
        draft.payload = payload
        draft.admission = MobileCommandAdmission(commandId: draft.messageId, issuedAt: draft.createdAt,
            expiresAt: draft.expiresAt, hostDeviceId: commandHostProvider?() ?? commandHostDeviceId,
            scaffold: commandScaffoldRoute.map(MobileCommandAdmission.ScaffoldAuthority.init))
        draft.admission?.scope = CollaborationScope(projectId: config.projectScope, deploymentId: deploymentId, sessionId: chatId)
        Task { await submitControl(draft) }
    }

    // Read only the original host/scope. Absence or an unavailable response is
    // not rejection; a retry keeps its durable identity and immutable request.
    private func matchesAdmissionPayload(_ command: [String: Any], draft: SubmittedDraft) -> Bool {
        guard let original = draft.payload, let admission = draft.admission,
              var payload = command["payload"] as? [String: Any] else { return false }
        let scoped = admission.scaffold != nil
        if let authority = admission.scaffold {
            guard payload["kind"] as? String == "control",
                  payload["sessionId"] as? String == authority.projection.sessionId,
                  payload["ownerDeviceId"] as? String == authority.ownerDeviceId,
                  payload["actorDeviceId"] as? String == authority.controllerDeviceId,
                  payload["actorSubject"] as? String == authority.actorSubject,
                  payload["grantId"] as? String == authority.grantId,
                  payload["source"] as? String == "scaffold",
                  let action = payload["action"] as? [String: Any] else { return false }
            payload = action
        }
        func decode<T: Decodable>(_ value: Any?, as type: T.Type) -> T? {
            guard let value, let data = try? JSONSerialization.data(withJSONObject: value) else { return nil }
            return try? JSONDecoder().decode(type, from: data)
        }
        let kind = payload[scoped ? "action" : "kind"] as? String
        switch original {
        case .run(let request, let id):
            guard kind == (scoped ? "start" : "run"), payload[scoped ? "message_id" : "messageId"] as? String == id,
                  let retained = decode(payload["request"], as: RunRequest.self) else { return false }
            return NSDictionary(dictionary: encodableDictionary(retained)).isEqual(to: encodableDictionary(request))
        case .steer(let prompt, let id):
            return kind == "steer" && payload["prompt"] as? String == prompt && payload[scoped ? "message_id" : "messageId"] as? String == id
        case .interrupt: return kind == (scoped ? "stop" : "interrupt")
        case .respondInput(let id, let answers):
            return kind == "respondInput" && payload[scoped ? "request_id" : "requestId"] as? String == id && decode(payload["answers"], as: [UserInputAnswer].self) == answers
        }
    }

    private func readAdmission(_ draft: inout SubmittedDraft, retryPending: Bool = false) async -> Bool? {
        // Readback can upgrade the durable receipt before returning nil to wake
        // a pending retry. Both send paths must resume with that upgraded copy.
        defer { if let retained = submittedDrafts[draft.messageId] { draft = retained } }
        guard let admission = draft.admission, let commandReader else { return nil }
        do {
            let response = try await commandReader(admission)
            guard let command = response["command"] as? [String: Any] else { return nil }
            guard command["commandId"] as? String == admission.commandId,
                  (command["issuedAt"] as? NSNumber)?.int64Value == admission.issuedAt,
                  (command["expiresAt"] as? NSNumber)?.int64Value == admission.expiresAt,
                  let status = command["status"] as? String else { return nil }
            guard matchesAdmissionPayload(command, draft: draft) else {
                reportSendFailure("Crew received a conflicting command receipt; the admission outcome is unknown. Its original payload and identity are retained; no retry was sent.", messageId: draft.messageId)
                return false
            }
            if ["rejected", "expired", "superseded", "cancelled"].contains(status) {
                reportSendFailure(command["resolution"] as? String ?? "Crew marked this instruction \(status).", messageId: draft.messageId, terminal: true)
                return false
            }
            guard ["pending", "applied"].contains(status) else { return nil }
            if status == "applied" { resolveAcceptedDraft(draft.messageId) }
            else if var retained = submittedDrafts[draft.messageId] {
                retained.admitted = true; retained.failure = nil
                submittedDrafts[draft.messageId] = retained
                retryDraft = nil
                clearSendFailure()
            }
            do { try persistDrafts() }
            catch {
                recoveryFailure = "Crew accepted this instruction, but could not checkpoint its receipt: \(error.localizedDescription). The original identity is retained."
                return true
            }
            if retryPending, status == "pending", admission.expiresAt > nowMs() { return nil }
            return true
        } catch { return nil }
    }

    private func submitControl(_ retained: SubmittedDraft) async {
        guard AppConfig.canonicalSessionId(chatId) != nil else {
            sendFailure = "Crew control requires a canonical public session identity. The instruction was not sent."
            return
        }
        guard !sending, !intentLoadBlocked, !retained.terminal else { return }
        guard submittedDrafts.count < 16 || submittedDrafts[retained.messageId] != nil else {
            sendFailure = "Crew retains 16 unresolved instructions. Review them before submitting another."
            return
        }
        sending = true
        defer { sending = false }
        var draft = retained
        submittedDrafts[draft.messageId] = draft
        do {
            if let accepted = await readAdmission(&draft, retryPending: true) { if accepted { clearSendFailure() }; return }
            guard draft.expiresAt > nowMs() else {
                reportSendFailure(draft.admissionAttempted ? "This original Crew control's retry window expired; its admission outcome is unknown. Its identity is retained for readback." : "This retained Crew instruction expired before admission; it was not sent.", messageId: draft.messageId, terminal: !draft.admissionAttempted)
                return
            }
            try persistDrafts()
            if !draft.admissionAttempted, draft.admission?.scaffold == nil, let route = try await commandRouteProvider?() {
                draft.admission?.scaffold = MobileCommandAdmission.ScaffoldAuthority(route)
                submittedDrafts[draft.messageId] = draft
                try persistDrafts()
            }
            guard let commandSender, let payload = draft.payload, let admission = draft.admission,
                  admission.expiresAt > nowMs() else { throw MobileSessionError.unavailable("This Crew instruction has no valid command route or has expired.") }
            draft.admissionAttempted = true
            submittedDrafts[draft.messageId] = draft
            try persistDrafts()
            try await commandSender(payload, admission)
            if var accepted = submittedDrafts[draft.messageId], !accepted.terminal {
                accepted.admitted = true; accepted.failure = nil
                submittedDrafts[draft.messageId] = accepted
                try persistDrafts()
            }
        } catch {
            if let accepted = await readAdmission(&draft), accepted { return }
            if submittedDrafts[draft.messageId] != nil, submittedDrafts[draft.messageId]?.admitted != true {
                if let relay = error as? RelayError, case .rpc(let detail) = relay,
                   ["command_id_conflict", "peer_command_scope_denied"].contains(where: { detail.contains($0) }) {
                    reportSendFailure("Crew rejected this control admission attempt: \(detail). Its original identity is retained.", messageId: draft.messageId)
                } else {
                    reportSendFailure(draft.admissionAttempted ? "Crew control admission outcome is unknown. Retry checks its original identity: \(error.localizedDescription)" : "Crew did not send this control: \(error.localizedDescription)", messageId: draft.messageId)
                }
            }
        }
    }
}

#if DEBUG
extension SessionStore {
    static func runAdmissionReadbackRegression() async -> Bool {
        let config = AppConfig(edgeURL: URL(string: "http://127.0.0.1:1")!, mode: .dev,
            userId: "admission-\(UUID().uuidString)", projectScope: "admission-regression", deviceId: "phone", deviceName: "Crew regression")
        let chatId = UUID().uuidString.lowercased()
        let initial = SessionStore(chatId: chatId, config: config)
        let cacheId = initial.intentCacheId
        var restarted: SessionStore?
        defer {
            initial.stop(); restarted?.stop()
            let prefix = DocDisk.intentURL(for: cacheId).lastPathComponent
            if let files = try? FileManager.default.contentsOfDirectory(at: DocDisk.directory, includingPropertiesForKeys: nil) {
                for file in files where file.lastPathComponent.hasPrefix(prefix) { try? FileManager.default.removeItem(at: file) }
            }
        }
        var admissions = 0
        var status = "pending"
        var original: MobileCommandAdmission?
        initial.commandHostDeviceId = "owner"
        initial.commandSender = { _, admission in
            admissions += 1; original = admission
            throw MobileSessionError.unavailable("metadata failed after durable admission")
        }
        initial.commandReader = { admission in
            guard let original, admission.commandId == original.commandId else { return ["command": NSNull()] }
            return ["command": ["commandId": original.commandId, "issuedAt": original.issuedAt, "expiresAt": original.expiresAt,
                "status": status, "payload": ["kind": "steer", "prompt": "original instruction", "messageId": original.commandId]]]
        }
        guard await initial.sendSteer(prompt: "original instruction"), admissions == 1,
              initial.sendFailure == nil, let original,
              let retained = initial.submittedDrafts[original.commandId], retained.admitted else { return false }
        do {
            try initial.recordAdmissionReceipt(MobileCommandReceipt(commandId: original.commandId,
                metadataError: "metadata conflict", preparationError: nil), admission: original)
        } catch { return false }
        guard initial.recoveryFailure?.contains("Crew accepted") == true,
              initial.recoveryFailure?.contains("metadata conflict") == true, initial.sendFailure == nil else { return false }
        let validReader = initial.commandReader
        initial.commandReader = { _ in
            ["command": ["commandId": original.commandId, "issuedAt": original.issuedAt, "expiresAt": original.expiresAt,
                "status": "applied", "payload": ["kind": "steer", "prompt": "changed payload", "messageId": original.commandId]]]
        }
        guard !(await initial.sendSteer(prompt: "original instruction")), admissions == 1,
              initial.submittedDrafts[original.commandId]?.terminal == false else { return false }
        initial.commandReader = validReader
        initial.flushToDisk()
        let restored = SessionStore(chatId: chatId, config: config)
        restarted = restored
        restored.commandSender = { _, _ in admissions += 1; throw RelayError.timeout }
        restored.commandReader = initial.commandReader
        status = "applied"
        guard await restored.sendSteer(prompt: "original instruction"), admissions == 1,
              restored.submittedDrafts.isEmpty else { return false }
        // A crash between the outcome tombstone and hot-outbox checkpoint must
        // not restore a completed instruction as a new composer draft.
        do { try DocDisk.saveIntents([retained], id: cacheId) } catch { return false }
        let completed = SessionStore(chatId: chatId, config: config)
        defer { completed.stop() }
        return completed.submittedDrafts.isEmpty && completed.failedPrompt == nil && completed.composerText.isEmpty
    }

    static func runAttachmentJournalRegression() async -> Bool {
        let config = AppConfig(edgeURL: URL(string: "http://127.0.0.1:1")!, mode: .dev,
            userId: "attachment-\(UUID().uuidString)", projectScope: "attachments", deviceId: "phone", deviceName: "Crew regression")
        let preview = UIGraphicsImageRenderer(size: CGSize(width: 1, height: 1)).image { context in
            UIColor.red.setFill(); context.fill(CGRect(x: 0, y: 0, width: 1, height: 1))
        }
        guard var bytes = preview.pngData() else { return false }
        // Real decodable PNG bytes, padded to exercise the actual 20 MiB limit.
        bytes.append(Data(repeating: 0, count: 20 * 1024 * 1024 - bytes.count))
        guard UIImage(data: bytes) != nil else { return false }
        let image = MobileImageAttachment(id: UUID(), filename: "retained.png", bytes: bytes, preview: preview)
        var stores: [SessionStore] = []
        var cacheIds: [String] = []
        defer {
            stores.forEach { $0.stop() }
            if let files = try? FileManager.default.contentsOfDirectory(at: DocDisk.directory, includingPropertiesForKeys: nil) {
                for file in files where cacheIds.contains(where: { file.lastPathComponent.hasPrefix($0) }) { try? FileManager.default.removeItem(at: file) }
            }
        }
        do {
            for terminal in [false, true] {
                let chatId = UUID().uuidString.lowercased()
                let initial = SessionStore(chatId: chatId, config: config)
                stores.append(initial); cacheIds.append(initial.intentCacheId)
                var admission: MobileCommandAdmission?
                initial.attachmentUploader = { _ in ["/fixture/retained.png"] }
                initial.commandSender = { _, value in admission = value; throw RelayError.timeout }
                guard !(await initial.sendSteer(prompt: "original text", images: [image])), let admission else { return false }
                if terminal { initial.reportSendFailure("rejected", messageId: admission.commandId, terminal: true) }
                initial.retainComposer(text: "edited text only", images: [image])
                try initial.persistDrafts()
                let restarted = SessionStore(chatId: chatId, config: config)
                stores.append(restarted)
                guard !restarted.intentLoadBlocked, restarted.composerText == "edited text only",
                      restarted.composerImages.first?.bytes == bytes,
                      restarted.submittedDrafts[admission.commandId]?.images.first?.bytes == bytes,
                      restarted.submittedDrafts[admission.commandId]?.terminal == terminal else { return false }
                let journal = try Data(contentsOf: DocDisk.intentURL(for: initial.intentCacheId))
                var changed = bytes; changed[changed.count - 1] = 1
                let conflicting = MobileImageAttachment(id: image.id, filename: image.filename, bytes: changed, preview: preview)
                restarted.retainComposer(text: "edited text only", images: [conflicting])
                do { try restarted.persistDrafts(); return false } catch {}
                guard try Data(contentsOf: DocDisk.intentURL(for: initial.intentCacheId)) == journal else { return false }
                let distinct = MobileImageAttachment(id: UUID(), filename: image.filename, bytes: bytes, preview: preview)
                restarted.retainComposer(text: "edited text only", images: [distinct])
                do { try restarted.persistDrafts(); return false } catch {}
                guard try Data(contentsOf: DocDisk.intentURL(for: initial.intentCacheId)) == journal else { return false }
                let original = restarted.submittedDrafts[admission.commandId]!
                let forged = SubmittedDraft(messageId: "composer", prompt: "conflict", images: [conflicting], steer: false)
                try DocDisk.saveIntents([original, forged], id: initial.intentCacheId)
                let forgedJournal = try Data(contentsOf: DocDisk.intentURL(for: initial.intentCacheId))
                let blocked = SessionStore(chatId: chatId, config: config)
                stores.append(blocked)
                guard blocked.intentLoadBlocked, blocked.submittedDrafts.isEmpty,
                      try Data(contentsOf: DocDisk.intentURL(for: initial.intentCacheId)) == forgedJournal else { return false }
            }
            E2ERunner.log("OK Crew attachment journal: failed and uncertain 20 MiB send, text-only composer edit, exact-identity dedupe, conflict and distinct-byte-limit rejection before replacement")
            return true
        } catch { E2ERunner.log("FAIL Crew attachment journal: \(error)"); return false }
    }

    static func runTerminalControlRegression() async -> Bool {
        let config = AppConfig(edgeURL: URL(string: "http://127.0.0.1:1")!, mode: .dev,
            userId: "controls-\(UUID().uuidString)", projectScope: "controls", deviceId: "phone", deviceName: "Crew regression")
        let chatId = UUID().uuidString.lowercased()
        let store = SessionStore(chatId: chatId, config: config)
        var stores = [store]
        defer {
            stores.forEach { $0.stop() }
            if let files = try? FileManager.default.contentsOfDirectory(at: DocDisk.directory, includingPropertiesForKeys: nil) {
                for file in files where file.lastPathComponent.hasPrefix(store.intentCacheId) { try? FileManager.default.removeItem(at: file) }
            }
        }
        do {
            var rejectedIds: [String] = []
            store.commandSender = { _, admission in
                rejectedIds.append(admission.commandId)
                store.reportSendFailure("Crew rejected this control", messageId: admission.commandId, terminal: true)
            }
            for index in 0..<20 {
                var control = SubmittedDraft(messageId: UUID().uuidString.lowercased(), prompt: "control \(index)", images: [], steer: false)
                control.payload = index.isMultiple(of: 2) ? .interrupt : .respondInput(requestId: "question", answers: [])
                control.createdAt = nowMs() - (index.isMultiple(of: 2) ? 300_001 : 0)
                control.expiresAt = control.createdAt + 300_000
                control.admission = MobileCommandAdmission(commandId: control.messageId, issuedAt: control.createdAt,
                    expiresAt: control.expiresAt, hostDeviceId: "host", scaffold: nil)
                await store.submitControl(control)
                guard store.submittedDrafts.isEmpty, store.retainedControls.count == index + 1,
                      store.retainedControls.allSatisfy({ $0.terminal }), store.sendFailure != nil else { return false }
                let url = DocDisk.intentURL(for: store.intentCacheId).appendingPathExtension("\(control.messageId).outcome")
                let outcome = try JSONDecoder().decode(SubmittedDraft.self, from: Data(contentsOf: url))
                guard outcome.terminal, outcome.failure != nil else { return false }
            }
            guard rejectedIds.count == 10 else { return false }
            var uncertain = SubmittedDraft(messageId: UUID().uuidString.lowercased(), prompt: "uncertain Stop", images: [], steer: false)
            uncertain.payload = .interrupt
            uncertain.admission = MobileCommandAdmission(commandId: uncertain.messageId, issuedAt: uncertain.createdAt,
                expiresAt: uncertain.expiresAt, hostDeviceId: "host", scaffold: nil)
            store.commandSender = { _, _ in throw RelayError.timeout }
            await store.submitControl(uncertain)
            let restarted = SessionStore(chatId: chatId, config: config)
            stores.append(restarted)
            guard !restarted.intentLoadBlocked, restarted.submittedDrafts.count == 1,
                  restarted.submittedDrafts[uncertain.messageId]?.terminal == false,
                  restarted.retainedControls.filter({ $0.terminal }).count == 20 else { return false }
            var admitted: [String] = []
            restarted.commandSender = { _, admission in admitted.append(admission.commandId) }
            await restarted.submitControl(restarted.submittedDrafts[uncertain.messageId]!)
            guard admitted == [uncertain.messageId], await restarted.sendSteer(prompt: "fresh instruction after terminal controls"),
                  admitted.count == 2, admitted[1] != uncertain.messageId else { return false }
            E2ERunner.log("OK Crew terminal controls: 20 expired/rejected outcomes remain visible after restart, pending slots freed, uncertain control identity retry, fresh send admitted")
            return true
        } catch { E2ERunner.log("FAIL Crew terminal controls: \(error)"); return false }
    }

    static func runOwnerAnchorReplayRegression() -> Bool {
        do {
            let doc = LoroDoc()
            let chatId = UUID().uuidString.lowercased()
            func value(epoch: Int64, at: Int64, status: String) -> [String: Any] {
                ["chatId": chatId, "sessionId": chatId, "ownerSubject": "owner",
                 "ownerDeviceId": "comet-scaffold-sandbox-e\(epoch)", "source": "scaffold",
                 "createdAt": Int64(1), "updatedAt": at, "status": status,
                 "environment": ["ownerPrincipal": "owner", "source": ["kind": "scaffold", "sandbox_id": "sandbox", "lifecycle_epoch": epoch],
                                 "scope": ["projectId": "project", "deploymentId": "deployment", "sessionId": chatId], "databaseEnvironment": "local"]]
            }
            func record(_ value: [String: Any]) -> LoroValue {
                LoroValue.fromJSON(["kind": "agentSession", "publishedBy": "owner", "value": value])
            }
            func append(_ value: [String: Any]) throws {
                let row = try doc.getList(id: "publications").pushContainer(child: LoroMap())
                try row.insert(key: "record", v: record(value)); doc.commit()
            }
            try append(value(epoch: 1, at: 100, status: "working"))
            try append(value(epoch: 2, at: 10, status: "working"))
            let current = decodeProjection(from: doc, chatId: chatId, observedAt: nil)
            try append(value(epoch: 1, at: 999, status: "errored"))
            try doc.getMap(id: "agentSessions").insert(key: chatId, v: record(value(epoch: 2, at: 20, status: "idle"))); doc.commit()
            let replayed = decodeProjection(from: doc, chatId: chatId, observedAt: nil, publicationCache: current.publicationCache)
            guard replayed.session?.deviceId == "comet-scaffold-sandbox-e2", replayed.session?.updatedAt == 20,
                  replayed.session?.status == .idle, replayed.environment?.source.lifecycleEpoch == 2 else { return false }
            // Simulate a register arriving after its anchor was evicted.
            let missing = PublicationCache(count: doc.getList(id: "publications").len(), anchors: [:])
            let recovered = decodeProjection(from: doc, chatId: chatId, observedAt: nil, publicationCache: missing)
            guard recovered.session?.updatedAt == 20, recovered.environment?.source.lifecycleEpoch == 2 else { return false }
            try doc.getMap(id: "agentSessions").insert(key: chatId, v: record(value(epoch: 2, at: 30, status: "idle"))); doc.commit()
            let pulse = decodeProjection(from: doc, chatId: chatId, observedAt: nil, publicationCache: recovered.publicationCache)
            guard pulse.session?.updatedAt == 30, pulse.publicationCache?.count == recovered.publicationCache?.count else { return false }
            // Older phases and equal-time active phases cannot mask terminal state.
            try append(value(epoch: 2, at: 40, status: "errored"))
            try append(value(epoch: 2, at: 40, status: "working"))
            try append(value(epoch: 2, at: 39, status: "idle"))
            let terminal = decodeProjection(from: doc, chatId: chatId, observedAt: nil, publicationCache: pulse.publicationCache)
            guard terminal.session?.status == .errored, terminal.session?.updatedAt == 40 else { return false }
            E2ERunner.log("OK Crew owner anchors: e1 replay after e2, missing-anchor scan, heartbeat-only refresh, timestamp and terminal tie ordering")
            return true
        } catch { E2ERunner.log("FAIL Crew owner anchor replay: \(error)"); return false }
    }
    func presentRecoveryFixture() {
        handle(.recoveryBlocked("Crew recovery is blocked by conflicting local records. Your original records and unsent drafts are retained on this device; no instruction was resubmitted."))
    }

    func receiveAuthoritativeFixture(_ source: LoroDoc) throws {
        _ = try doc.importWith(bytes: source.export(mode: .snapshot), origin: "authoritative-fixture")
        handle(.connected)
    }

    static func runDeploymentRetargetRegression() -> Bool {
        let config = AppConfig(edgeURL: URL(string: "http://127.0.0.1:1")!, mode: .dev,
                               userId: "retarget-\(UUID().uuidString)", projectScope: "retarget",
                               deviceId: "phone", deviceName: "Crew regression")
        let chatId = UUID().uuidString.lowercased()
        let a = SessionStore(chatId: chatId, config: config, deploymentId: "A")
        let b = SessionStore(chatId: chatId, config: config, deploymentId: "B")
        let cacheIds = [a.intentCacheId, b.intentCacheId]
        defer {
            a.stop(); b.stop()
            for id in cacheIds {
                try? FileManager.default.removeItem(at: DocDisk.intentURL(for: id))
                try? FileManager.default.removeItem(at: DocDisk.url(for: id))
            }
        }
        do {
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.sortedKeys]
            for store in [a, b] {
                var draft = SubmittedDraft(messageId: UUID().uuidString.lowercased(),
                                           prompt: "instruction \(store.deploymentId!)", images: [], steer: true)
                draft.payload = .steer(prompt: draft.prompt, messageId: draft.messageId)
                draft.admission = MobileCommandAdmission(commandId: draft.messageId,
                    issuedAt: draft.createdAt, expiresAt: draft.expiresAt, hostDeviceId: "host", scaffold: nil,
                    scope: CollaborationScope(projectId: config.projectScope, deploymentId: store.deploymentId, sessionId: chatId))
                store.submittedDrafts[draft.messageId] = draft
                store.retainComposer(text: "composer \(store.deploymentId!)", images: [])
                store.flushToDisk()
            }
            b.stop()
            let aOriginal = a.submittedDrafts.values.first!
            let bOriginal = b.submittedDrafts.values.first!
            let bBytes = try Data(contentsOf: DocDisk.intentURL(for: cacheIds[1]))
            a.updateDeploymentId("B")
            let destinationRetained = try Data(contentsOf: DocDisk.intentURL(for: cacheIds[1])) == bBytes
            // No yield: cancel hydration before any fixture can dial a room.
            a.stop()
            guard destinationRetained, a.composerText == "composer B", a.failedPrompt == "instruction B",
                  let recoveredB = a.submittedDrafts[bOriginal.messageId],
                  try encoder.encode(recoveredB) == encoder.encode(bOriginal),
                  a.submittedDrafts[aOriginal.messageId] == nil else { return false }
            a.updateDeploymentId("A")
            a.stop()
            guard a.composerText == "composer A", a.failedPrompt == "instruction A",
                  let recoveredA = a.submittedDrafts[aOriginal.messageId],
                  try encoder.encode(recoveredA) == encoder.encode(aOriginal),
                  a.submittedDrafts[bOriginal.messageId] == nil else { return false }
            E2ERunner.log("OK Crew deployment retarget: distinct retained composers and original scoped instructions survive A B A")
            return true
        } catch { E2ERunner.log("FAIL Crew deployment retarget: \(error)"); return false }
    }

    static func runDurableIntentRegression() async -> Bool {
        let config = AppConfig(edgeURL: URL(string: "http://127.0.0.1:1")!, mode: .dev,
            userId: "durable-\(UUID().uuidString)", projectScope: "durable-regression",
            deviceId: "phone", deviceName: "Crew regression")
        let chatId = UUID().uuidString.lowercased()
        let cacheId = config.documentCacheId(roomId: chatId)
        var stores: [SessionStore] = []
        defer {
            stores.forEach { $0.stop() }
            if let files = try? FileManager.default.contentsOfDirectory(at: DocDisk.directory, includingPropertiesForKeys: nil) {
                for file in files where file.lastPathComponent.hasPrefix(cacheId) { try? FileManager.default.removeItem(at: file) }
            }
        }
        var checkpoint = "cached transcript and original composer"
        var completed = false
        defer { if !completed { E2ERunner.log("FAIL Crew durable intents checkpoint: \(checkpoint)") } }
        do {
            let old = LoroDoc()
            func append(_ fields: [String: Any], to doc: LoroDoc, list: String) throws {
                let row = try doc.getList(id: list).pushContainer(child: LoroMap())
                for (key, value) in fields { try row.insert(key: key, v: LoroValue.fromJSON(value)) }
            }
            try append([
                "id": "old-message", "role": "user", "createdAt": Int64(1), "deviceId": "owner",
                "parts": [["id": "text", "kind": "text", "text": "old cached history"]]
            ], to: old, list: "messages")
            old.commit()
            DocDisk.save(doc: old, id: cacheId)
            let initial = SessionStore(chatId: chatId, config: config)
            stores.append(initial)
            initial.retainComposer(text: "unsent composer draft", images: [])
            var admissions: [MobileCommandAdmission] = []
            initial.commandSender = { _, admission in admissions.append(admission); throw RelayError.timeout }
            let chat = Chat(id: chatId, deviceId: "owner", archived: false, cwd: "/original", createdAt: 0)
            checkpoint = "original run lost admission reply"
            guard !(await initial.sendRun(prompt: "accepted but ACK lost", chat: chat)),
                  admissions.count == 1 else { return false }
            initial.flushToDisk()
            let restarted = SessionStore(chatId: chatId, config: config)
            stores.append(restarted)
            checkpoint = "restart restores composer and original scoped request"
            guard restarted.composerText == "unsent composer draft",
                  restarted.failedPrompt == "accepted but ACK lost",
                  let original = admissions.first,
                  restarted.submittedDrafts[original.commandId]?.request?.cwd == "/original",
                  let cached = DocDisk.loadReplica(id: cacheId) else { return false }
            func command(_ payload: [String: Any], admission: MobileCommandAdmission, status: String = "pending") -> [String: Any] {
                ["commandId": admission.commandId, "issuedAt": admission.issuedAt,
                 "expiresAt": admission.expiresAt, "status": status, "payload": payload]
            }
            checkpoint = "retained run request"
            guard let retainedRun = restarted.submittedDrafts[original.commandId],
                  let originalRequest = retainedRun.request else { return false }
            var nativeRequest = encodableDictionary(originalRequest)
            nativeRequest.removeValue(forKey: "attachments")
            let runPayload: [String: Any] = ["kind": "run", "messageId": original.commandId, "request": nativeRequest]
            let pendingRun = command(runPayload, admission: original)
            checkpoint = "native omitted-empty attachments match original run"
            guard restarted.matchesAdmissionPayload(pendingRun, draft: retainedRun) else { return false }
            for changed in [NSNull(), "/not-an-array", ["/changed-image"]] as [Any] {
                var request = nativeRequest; request["attachments"] = changed
                checkpoint = "malformed or changed attachments refused"
                guard !restarted.matchesAdmissionPayload(command(["kind": "run", "messageId": original.commandId, "request": request], admission: original), draft: retainedRun) else { return false }
            }
            var changedRequest = nativeRequest; changedRequest["cwd"] = "/changed"
            checkpoint = "changed run working directory refused"
            guard !restarted.matchesAdmissionPayload(command(["kind": "run", "messageId": original.commandId, "request": changedRequest], admission: original), draft: retainedRun) else { return false }
            var scopedRun = retainedRun
            let route = ScaffoldControlRoute(controllerDeviceId: "controller", ownerDeviceId: "owner",
                actorSubject: config.userId, grantId: "original-grant",
                projection: SessionRoomProjection(projectId: config.projectScope, deploymentId: "scaffold", sessionId: chatId),
                environment: SessionEnvironment(source: SessionEnvironmentSource(kind: "scaffold"), ownerPrincipal: config.userId,
                    scope: CollaborationScope(projectId: config.projectScope, deploymentId: "scaffold", sessionId: chatId)))
            scopedRun.admission?.scaffold = MobileCommandAdmission.ScaffoldAuthority(route)
            func scaffoldPayload(_ request: [String: Any]) -> [String: Any] {
                ["kind": "control", "sessionId": chatId, "ownerDeviceId": route.ownerDeviceId,
                 "actorDeviceId": route.controllerDeviceId, "actorSubject": route.actorSubject,
                 "grantId": route.grantId, "source": "scaffold",
                 "action": ["action": "start", "message_id": original.commandId, "request": request]]
            }
            checkpoint = "scoped original run accepted and changed request refused"
            guard restarted.matchesAdmissionPayload(command(scaffoldPayload(nativeRequest), admission: original), draft: scopedRun),
                  !restarted.matchesAdmissionPayload(command(scaffoldPayload(changedRequest), admission: original), draft: scopedRun) else { return false }
            var runReadbacks = 0
            restarted.commandReader = { _ in
                runReadbacks += 1
                guard runReadbacks == 1 else { throw RelayError.timeout }
                return ["command": pendingRun]
            }
            restarted.doc = cached
            restarted.commandSender = { payload, admission in
                guard case .run(let request, let id) = payload, id == original.commandId, request.cwd == "/original" else {
                    throw MobileSessionError.unavailable("retry mutated the original request")
                }
                admissions.append(admission)
                throw RelayError.timeout
            }
            checkpoint = "pending readback retries original run identity after restart"
            guard await restarted.sendSteer(prompt: "accepted but ACK lost"), admissions.count == 2,
                  admissions[1].commandId == original.commandId,
                  admissions[1].issuedAt == original.issuedAt,
                  admissions[1].expiresAt == original.expiresAt else { return false }
            let pendingRestart = SessionStore(chatId: chatId, config: config)
            stores.append(pendingRestart)
            checkpoint = "pending run receipt survives lost retry reply and restart"
            guard runReadbacks == 2, let pendingDraft = pendingRestart.submittedDrafts[original.commandId],
                  pendingDraft.admitted, !pendingDraft.terminal, pendingDraft.failure == nil,
                  let pendingRequest = pendingDraft.request, let pendingAdmission = pendingDraft.admission,
                  NSDictionary(dictionary: encodableDictionary(pendingRequest)).isEqual(to: encodableDictionary(originalRequest)),
                  NSDictionary(dictionary: encodableDictionary(pendingAdmission)).isEqual(to: encodableDictionary(original)),
                  pendingRestart.composerText == "unsent composer draft" else { return false }
            let server = old.fork()
            try append([
                "id": original.commandId, "role": "user", "createdAt": original.issuedAt, "deviceId": "owner",
                "parts": [["id": "text", "kind": "text", "text": "accepted but ACK lost"]]
            ], to: server, list: "messages")
            server.commit()
            let floor = server.stateFrontiers()
            try server.getMap(id: "meta").insert(key: "current", v: true)
            server.commit()
            checkpoint = "independently materialized shallow replacement"
            guard let replacement = DocDisk.replacementSnapshot(bytes: try server.export(mode: .shallowSnapshot(frontiers: floor))) else { return false }
            let room = RoomClient(roomId: chatId, doc: cached, urlProvider: { nil }, events: { _ in }, adoptSnapshot: { _, _ in false })
            restarted.room = room
            restarted.saver = DocSaver(docId: cacheId, doc: cached)
            checkpoint = "shallow adoption retains original materialization without re-admission"
            guard restarted.adoptSnapshot(previous: cached, replacement: replacement),
                  await E2ERunner.poll(timeout: 5, label: "durable recovery materialization", {
                    restarted.entries.contains(where: { $0.id == original.commandId }) ? true : nil
                  }) != nil,
                  await restarted.sendSteer(prompt: "accepted but ACK lost"), admissions.count == 2,
                  DocDisk.loadReplica(id: cacheId)?.getMap(id: "meta").get(key: "current")?.asValue()?.boolValue == true,
                  FileManager.default.fileExists(atPath: DocDisk.url(for: cacheId).appendingPathExtension("recovery").path) else { return false }
            let afterRecovery = SessionStore(chatId: chatId, config: config)
            stores.append(afterRecovery)
            checkpoint = "completed original outcome survives restart with unsent composer"
            guard afterRecovery.submittedDrafts.isEmpty,
                  afterRecovery.composerText == "unsent composer draft" else { return false }
            var expired = SubmittedDraft(messageId: UUID().uuidString.lowercased(), prompt: "expired instruction", images: [], steer: true)
            expired.createdAt = nowMs() - 86_400_001; expired.expiresAt = expired.createdAt + 86_400_000
            expired.payload = .steer(prompt: expired.prompt, messageId: expired.messageId)
            afterRecovery.submittedDrafts[expired.messageId] = expired
            afterRecovery.retryDraft = expired
            afterRecovery.commandSender = { _, admission in admissions.append(admission) }
            checkpoint = "expired unsent instruction is terminal without admission"
            guard !(await afterRecovery.sendSteer(prompt: expired.prompt)), admissions.count == 2,
                  afterRecovery.submittedDrafts[expired.messageId]?.terminal == true else { return false }
            // A rejected ledger result after restart must be terminal even when
            // the transient optimistic echo did not survive the process.
            var revoked = SubmittedDraft(messageId: UUID().uuidString.lowercased(), prompt: "revoked instruction", images: [], steer: true)
            revoked.payload = .steer(prompt: revoked.prompt, messageId: revoked.messageId)
            afterRecovery.submittedDrafts[revoked.messageId] = revoked
            try append([
                "id": revoked.messageId, "status": "rejected", "resolution": "grant revoked",
                "payload": ["kind": "steer", "messageId": revoked.messageId, "prompt": revoked.prompt]
            ], to: afterRecovery.doc, list: "commands")
            afterRecovery.doc.commit()
            afterRecovery.project()
            checkpoint = "revoked durable instruction is terminal without optimistic echo"
            guard await E2ERunner.poll(timeout: 5, label: "revoked durable intent", {
                afterRecovery.submittedDrafts[revoked.messageId]?.terminal == true ? true : nil
            }) != nil, admissions.count == 2 else { return false }
            var controlAdmissions: [MobileCommandAdmission] = []
            let answers = [UserInputAnswer(questionId: "question", labels: ["original answer"])]
            afterRecovery.commandSender = { payload, admission in
                guard case .respondInput(let requestId, let retainedAnswers) = payload,
                      requestId == "original-request", retainedAnswers == answers else {
                    throw MobileSessionError.unavailable("control retry changed original answers")
                }
                controlAdmissions.append(admission)
                throw RelayError.timeout
            }
            afterRecovery.respondInput(requestId: "original-request", answers: answers)
            checkpoint = "input response admission loss retains original answers"
            guard await E2ERunner.poll(timeout: 5, label: "durable control admission loss", {
                controlAdmissions.count == 1 && afterRecovery.retainedControls.count == 1 ? true : nil
            }) != nil, let controlAdmission = controlAdmissions.first else { return false }
            afterRecovery.flushToDisk()
            let controlRestart = SessionStore(chatId: chatId, config: config)
            stores.append(controlRestart)
            controlRestart.commandSender = afterRecovery.commandSender
            let pendingControl = command(["kind": "respondInput", "requestId": "original-request",
                "answers": answers.map(encodableDictionary)], admission: controlAdmission)
            var controlReadbacks = 0
            controlRestart.commandReader = { _ in
                controlReadbacks += 1
                guard controlReadbacks == 1 else { throw RelayError.timeout }
                return ["command": pendingControl]
            }
            controlRestart.retryControl(controlAdmission.commandId)
            checkpoint = "input retry retains original command identity and expiry"
            guard await E2ERunner.poll(timeout: 5, label: "durable control retry", {
                controlAdmissions.count == 2 && !controlRestart.sending ? true : nil
            }) != nil,
                  controlAdmissions[1].commandId == controlAdmission.commandId,
                  controlAdmissions[1].issuedAt == controlAdmission.issuedAt,
                  controlAdmissions[1].expiresAt == controlAdmission.expiresAt else { return false }
            let pendingControlRestart = SessionStore(chatId: chatId, config: config)
            stores.append(pendingControlRestart)
            checkpoint = "pending input receipt survives lost retry reply and restart"
            guard controlReadbacks == 2, let pendingControlDraft = pendingControlRestart.submittedDrafts[controlAdmission.commandId],
                  pendingControlDraft.admitted, !pendingControlDraft.terminal, pendingControlDraft.failure == nil,
                  let retainedControlAdmission = pendingControlDraft.admission,
                  NSDictionary(dictionary: encodableDictionary(retainedControlAdmission)).isEqual(to: encodableDictionary(controlAdmission)) else { return false }
            try append(["id": controlAdmission.commandId, "status": "applied", "payload": ["kind": "respondInput"]],
                       to: controlRestart.doc, list: "commands")
            controlRestart.doc.commit(); controlRestart.project()
            checkpoint = "applied input outcome retires retained control"
            guard await E2ERunner.poll(timeout: 5, label: "durable control outcome", {
                controlRestart.submittedDrafts[controlAdmission.commandId] == nil ? true : nil
            }) != nil else { return false }
            let terminalRestart = SessionStore(chatId: chatId, config: config)
            stores.append(terminalRestart)
            checkpoint = "applied control stays retired after restart"
            guard terminalRestart.submittedDrafts[controlAdmission.commandId] == nil else { return false }
            var cancelled = pendingControlDraft
            cancelled.messageId = UUID().uuidString.lowercased()
            cancelled.admission?.commandId = cancelled.messageId
            checkpoint = "cancelled control retains original admission"
            guard let cancelledAdmission = cancelled.admission else { return false }
            terminalRestart.submittedDrafts[cancelled.messageId] = cancelled
            let cancelledCommand = command(["kind": "respondInput", "requestId": "original-request",
                "answers": answers.map(encodableDictionary)], admission: cancelledAdmission, status: "cancelled")
            terminalRestart.commandReader = { _ in ["command": cancelledCommand] }
            await terminalRestart.submitControl(cancelled)
            let cancelledRestart = SessionStore(chatId: chatId, config: config)
            stores.append(cancelledRestart)
            cancelledRestart.commandSender = afterRecovery.commandSender
            cancelledRestart.retryControl(cancelled.messageId)
            checkpoint = "cancelled control stays terminal after restart and retry"
            guard cancelledRestart.submittedDrafts[cancelled.messageId] == nil,
                  cancelledRestart.terminalControlOutcomes[cancelled.messageId]?.terminal == true,
                  controlAdmissions.count == 2 else { return false }
            let wrongScope = SessionStore(chatId: chatId, config: config, deploymentId: "another-deployment")
            stores.append(wrongScope)
            checkpoint = "foreign deployment cannot recover retained composer or commands"
            guard wrongScope.submittedDrafts.isEmpty, wrongScope.composerText.isEmpty else { return false }
            let scope = CollaborationScope(projectId: config.projectScope, deploymentId: "another-deployment", sessionId: chatId)
            let issuedAt = nowMs()
            let scopedAdmission = MobileCommandAdmission(commandId: UUID().uuidString.lowercased(),
                issuedAt: issuedAt, expiresAt: issuedAt + 300_000, hostDeviceId: "host", scaffold: nil, scope: scope)
            let workspace = WorkspaceStore(config: config)
            defer { workspace.stop(); try? FileManager.default.removeItem(at: DocDisk.intentURL(for: config.documentCacheId(roomId: "ws4/\(config.projectScope)"))) }
            checkpoint = "desktop admission refuses implicit deployment change"
            do {
                try await workspace.sendSessionCommand(chatId: chatId, payload: .interrupt, admission: scopedAdmission)
                return false
            } catch {
                guard error.localizedDescription.contains("explicit deployment") else { return false }
            }
            completed = true
            E2ERunner.log("OK Crew durable intents: restart draft, original ID payload expiry, lost ACK dedupe, shallow adoption, revoked and expired blocked, scope isolation, durable control answers and outcomes, native omitted-empty attachments, pending receipt survives lost retry and restart, cancelled outcomes monotonic")
            return true
        } catch { E2ERunner.log("FAIL Crew durable intents: \(error)"); return false }
    }
}
#endif

#if DEBUG
extension SessionStore {
    static func runTranscriptActivityRegression() async -> Bool {
        let config = AppConfig(edgeURL: URL(string: "http://127.0.0.1:1")!, mode: .dev,
            userId: "activity-\(UUID().uuidString)", projectScope: "activity", deviceId: "viewer", deviceName: "Crew regression")
        let store = SessionStore(chatId: UUID().uuidString.lowercased(), config: config)
        let cacheId = store.intentCacheId
        defer {
            store.stop()
            try? FileManager.default.removeItem(at: DocDisk.intentURL(for: cacheId))
        }
        do {
            let stale = nowMs() - sessionStaleMs - 1
            let row = try store.doc.getList(id: "messages").pushContainer(child: LoroMap())
            try row.insert(key: "id", v: "stream"); try row.insert(key: "role", v: "assistant")
            try row.insert(key: "deviceId", v: "host"); try row.insert(key: "status", v: "streaming")
            try row.insert(key: "createdAt", v: stale)
            try row.insert(key: "parts", v: LoroValue.fromJSON([["id": "text", "kind": "text", "text": "old stream"]]))
            store.doc.commit(); store.project()
            guard await E2ERunner.poll(timeout: 5, label: "stale transcript", { store.transcriptActivity != nil ? true : nil }) != nil else { return false }
            try store.doc.getMap(id: "meta").insert(key: "ownerHeartbeat", v: nowMs())
            store.doc.commit(); store.handle(.remoteUpdate)
            guard await E2ERunner.poll(timeout: 5, label: "status-only transcript projection", { !store.isProjecting ? true : nil }) != nil,
                  store.transcriptActivity?.updatedAt == stale,
                  effectiveStatus(store.transcriptActivity, now: nowMs()) == nil else { return false }
            try row.insert(key: "parts", v: LoroValue.fromJSON([["id": "text", "kind": "text", "text": "actual new streamed content"]]))
            store.doc.commit(); store.handle(.remoteUpdate)
            guard await E2ERunner.poll(timeout: 5, label: "actual transcript progress", {
                effectiveStatus(store.transcriptActivity, now: nowMs()) == .working ? true : nil
            }) != nil else { return false }
            let timestamp = store.transcriptActivity!.updatedAt
            let published = SessionRow(chatId: store.chatId, deviceId: "host", status: .working, startedAt: stale, updatedAt: stale)
            guard sessionActivity(workspace: nil, published: published, transcript: store.transcriptActivity,
                                  now: timestamp + sessionStaleMs + 1).status == nil else { return false }
            E2ERunner.log("OK Crew transcript freshness: metadata heartbeat cannot revive stale stream, actual content progress becomes fresh, stale owner and stream unreachable")
            return true
        } catch { E2ERunner.log("FAIL Crew transcript freshness: \(error)"); return false }
    }
}
#endif
