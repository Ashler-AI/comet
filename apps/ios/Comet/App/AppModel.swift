// App session root: sign-in state machine, workspace connection, and the
// per-chat session store cache. Also hosts demo mode — an offline in-memory
// dataset so the UI can be exercised without an edge deployment.

import Foundation
import Observation
import SwiftUI

enum MobileSessionError: LocalizedError {
    case unavailable(String)

    var errorDescription: String? {
        switch self { case .unavailable(let message): return message }
    }
}

@MainActor
@Observable
final class AppModel {
    enum Phase {
        case signedOut
        case ready
    }
    private var scaffoldRoutes: [String: ScaffoldControlRoute] = [:]

    var phase: Phase = .signedOut
    var workspace: WorkspaceStore?
    var demo: DemoDataset?
    private var demoSessionRefs: [SessionRef] = []
    private var sessionStores: [String: SessionStore] = [:]
    @ObservationIgnored private var metadataReaders: [String: Int] = [:]
    @ObservationIgnored private var metadataOnlyStores: Set<String> = []
    @ObservationIgnored private var recentTranscriptIds: [String] = []
    @ObservationIgnored private var pendingMetadataReleases: Set<String> = []
    @ObservationIgnored private var metadataReleaseTask: Task<Void, Never>?
    @ObservationIgnored private var metadataReleaseDeadline: UInt64?
    private struct ListMetadata {
        var deploymentId: String?
        var environment: SessionEnvironment?
        var previewTitle: String?
        var session: SessionRow?
    }
    private var listMetadata: [String: ListMetadata] = [:]
    private var config: AppConfig?
    let notifications = SessionNotifications.shared

    // Persisted connection settings.
    @ObservationIgnored @AppStorage("edgeURL") var edgeURLString = ReleaseConfig.edgeURL.absoluteString
    @ObservationIgnored @AppStorage("authMode") var authModeRaw = AppConfig.Mode.scaffold.rawValue
    @ObservationIgnored @AppStorage("userId") var storedUserId = ""
    @ObservationIgnored @AppStorage("projectScope") var storedProjectScope = ""
    @ObservationIgnored @AppStorage("deviceId") var storedDeviceId = ""

    var deviceId: String {
        if storedDeviceId.isEmpty {
            storedDeviceId = "ios-" + UUID().uuidString.lowercased().prefix(8)
        }
        return storedDeviceId
    }

    var deviceName: String {
        UIDevice.current.name
    }

    /// Deep-link target applied by HomeView on first appearance (set by launch
    /// args in demo mode; simulator-driven screenshots use it).
    var launchRoute: Route?
    /// Invitation accepted before the workspace connected (cold-start URL);
    /// pinned and routed the moment `phase` reaches `.ready`.
    private var pendingInviteChatId: String?
    /// Screenshot rig: "newsession" / "newspace" presents that sheet on arrival.
    var launchSheet: String?
    /// Screenshot rig: auto-send a canned prompt from the new-session canvas.
    var launchAutosend = false

    func restore() {
        if demo != nil || workspace != nil { return }
        DocDisk.prune(keep: 80)
        let args = ProcessInfo.processInfo.arguments
        // Debug-rig config overrides (cfprefsd caching defeats external
        // defaults writes; the app applying them itself always sticks).
        func override(_ flag: String, _ apply: (String) -> Void) {
            if let ix = args.firstIndex(of: flag), ix + 1 < args.count {
                apply(args[ix + 1])
            }
        }
        override("-setedge") { edgeURLString = $0 }
        override("-setmode") { authModeRaw = $0 }
        override("-setuser") { storedUserId = $0 }
        override("-setproject") { storedProjectScope = $0 }
        if args.contains("-visibility-e2e") {
            E2ERunner.runSessionVisibility()
            E2ERunner.runAttentionTransitions()
            Task {
                await E2ERunner.runMobileParity()
                await E2ERunner.runStoreEviction()
            }
            #if DEBUG
            Task { await SessionNotifications.runLifecycleRegression() }
            #endif
            enterDemoMode()
            return
        }
        if args.contains("-bench") {
            Task { await BenchRunner.run() }
            return
        }
        if args.contains("-e2e") {
            Task { await E2ERunner.run(model: self) }
            return
        }
        if args.contains("-e2e-live") {
            // Reuse the signed-in session, then probe the live relay paths.
            Task {
                try? await Task.sleep(nanoseconds: 500_000_000)
                await E2ERunner.runLive(model: self)
            }
            // fall through to the normal restore below
        }
        if args.contains("-demo") {
            enterDemoMode()
            if args.contains("-large-list"), let demo { BenchRunner.populateList(demo: demo) }
            if let ix = args.firstIndex(of: "-route"), ix + 1 < args.count {
                let spec = args[ix + 1]
                if spec.hasPrefix("chat:") {
                    let chatId = String(spec.dropFirst("chat:".count))
                    launchRoute = .chat(chatId)
                    if args.contains("-big"), let demo {
                        // Scroll-settle stress. Injected BEFORE the transcript
                        // appears, which is the warm-session case: rows are
                        // already there at first layout, so neither the
                        // rows-arrived nor the streamed-growth anchor ever
                        // fires and `.task` is the only thing holding the
                        // bottom — against hundreds of lazily-estimated rows.
                        demo.sessionStore(for: chatId)
                            .setEntries(BenchRunner.syntheticEntries(turns: 120))
                    }
                    if args.contains("-stream"), let demo {
                        // Screenshot rig: kick off the scripted streaming reply.
                        let store = demo.sessionStore(for: chatId)
                        Task { @MainActor in
                            try? await Task.sleep(nanoseconds: 2_000_000_000)
                            store.demoResponder?("Show me the streamed reply path.")
                        }
                    }
                } else if spec.hasPrefix("space:") {
                    launchRoute = .space(String(spec.dropFirst("space:".count)))
                }
            }
            if let ix = args.firstIndex(of: "-sheet"), ix + 1 < args.count {
                launchSheet = args[ix + 1]
            }
            launchAutosend = args.contains("-autosend")
            return
        }
        guard let url = URL(string: edgeURLString),
              !storedUserId.isEmpty,
              !storedProjectScope.isEmpty else { return }
        let mode = AppConfig.Mode(rawValue: authModeRaw) ?? .scaffold
        switch mode {
        case .dev:
            connect(url: url, mode: .dev, userId: storedUserId,
                    projectScope: storedProjectScope, tokens: nil,
                    devBearer: devBearer(userId: storedUserId, projectScope: storedProjectScope))
        case .scaffold:
            guard let access = Keychain.load(key: "accessToken") else { return }
            connect(url: url, mode: .scaffold, userId: storedUserId,
                    projectScope: storedProjectScope,
                    tokens: AuthTokens(accessToken: access), devBearer: nil)
        }
    }

    // MARK: Sign-in flows

    func beginSignIn(scaffoldURL: URL, redirectURI: String) async throws -> OAuthFlow {
        try await AuthClient(scaffoldURL: scaffoldURL).beginSignIn(redirectURI: redirectURI)
    }

    func completeSignIn(
        edgeURL: URL,
        scaffoldURL: URL,
        projectScope: String,
        flow: OAuthFlow,
        callbackURL: URL
    ) async throws {
        let (user, tokens) = try await AuthClient(scaffoldURL: scaffoldURL)
            .completeSignIn(flow: flow, callbackURL: callbackURL)
        Keychain.save(tokens.accessToken, key: "accessToken")
        edgeURLString = edgeURL.absoluteString
        authModeRaw = AppConfig.Mode.scaffold.rawValue
        storedUserId = user.id
        storedProjectScope = projectScope
        connect(url: edgeURL, mode: .scaffold, userId: user.id,
                projectScope: projectScope, tokens: tokens, devBearer: nil)
    }

    /// Local development edge: bearer = "userId@projectScope".
    func signInDev(edgeURL: URL, userId: String, projectScope: String) {
        edgeURLString = edgeURL.absoluteString
        authModeRaw = AppConfig.Mode.dev.rawValue
        storedUserId = userId
        storedProjectScope = projectScope
        connect(url: edgeURL, mode: .dev, userId: userId, projectScope: projectScope,
                tokens: nil, devBearer: devBearer(userId: userId, projectScope: projectScope))
    }

    func enterDemoMode() {
        demo = DemoDataset.standard()
        phase = .ready
        drainPendingInvite()
    }

    func signOut() {
        notifications.signOut()
        workspace?.stop()
        workspace = nil
        sessionStores.values.forEach { $0.stop() }
        sessionStores.removeAll()
        metadataReaders.removeAll()
        metadataOnlyStores.removeAll()
        metadataReleaseTask?.cancel()
        metadataReleaseTask = nil
        metadataReleaseDeadline = nil
        pendingMetadataReleases.removeAll()
        recentTranscriptIds.removeAll()
        listMetadata.removeAll()
        scaffoldRoutes.removeAll()
        demoSessionRefs.removeAll()
        config = nil
        demo = nil
        Keychain.delete(key: "accessToken")
        DocDisk.wipeAll()  // local doc state belongs to the signed-in identity
        storedUserId = ""
        storedProjectScope = ""
        phase = .signedOut
    }

    private func devBearer(userId: String, projectScope: String) -> String {
        projectScope.isEmpty ? userId : "\(userId)@\(projectScope)"
    }

    private func connect(url: URL, mode: AppConfig.Mode, userId: String, projectScope: String,
                         tokens: AuthTokens?, devBearer: String?) {
        let config = AppConfig(edgeURL: url, mode: mode, userId: userId,
                               projectScope: projectScope, deviceId: deviceId,
                               deviceName: deviceName, tokens: tokens, devBearer: devBearer)
        self.config = config
        notifications.configure(config) { [weak self] chatId in
            self?.launchRoute = .chat(chatId)
        }
        let store = WorkspaceStore(config: config)
        store.onProjection = { [weak self] in
            guard let self, let workspace = self.workspace else { return }
            self.notifications.update(sessions: workspace.sessions, chats: workspace.chats) { chatId in
                guard let chat = workspace.chat(id: chatId) else { return nil }
                return self.sessionTitle(for: chat, fallbackTitle: "Session \(chatId.prefix(8))")
            }
            self.preloadSessionMetadata()
        }
        workspace = store
        store.start()
        phase = .ready
        drainPendingInvite()
    }

    // MARK: Unified data accessors (demo or live — one path for views)

    var spaces: [Space] { demo?.spaces ?? workspace?.spaces ?? [] }
    var occupiedSpaces: [Space] { demo?.lists.occupiedSpaces ?? workspace?.occupiedSpaces ?? [] }

    func space(id: String) -> Space? { demo?.lists.spacesById[id] ?? workspace?.space(id: id) }

    var connected: Bool { demo != nil || workspace?.connected == true }

    var overviewChats: [Chat] {
        demo?.overviewChats ?? workspace?.overviewChats ?? []
    }

    var settledChats: [Chat] {
        demo?.settledChats ?? workspace?.settledChats ?? []
    }

    /// Imported memberships with no chat row — a chat row always wins and
    /// renders with full context in Sessions or Archived sessions.
    var sharedSessionRefs: [SessionRef] {
        if let demo {
            return demoSessionRefs.filter { demo.lists.chatsById[$0.chatId] == nil }
        }
        return workspace?.sharedSessionRefs ?? []
    }

    func sessionRef(id: String) -> SessionRef? {
        if let demo {
            guard demo.lists.chatsById[id] == nil else { return nil }
            return demoSessionRefs.first { $0.chatId == id }
        }
        guard workspace?.chat(id: id) == nil else { return nil }
        return workspace?.sessionRef(id: id)
    }

    @discardableResult
    func addSessionRef(_ chatId: String) -> SessionRef? {
        if demo != nil {
            if let existing = demoSessionRefs.first(where: { $0.chatId == chatId }) {
                return existing
            }
            let ref = SessionRef(chatId: chatId, addedAt: nowMs(), environment: nil)
            demoSessionRefs.insert(ref, at: 0)
            return ref
        }
        return workspace?.addSessionRef(chatId: chatId)
    }

    func removeSessionRef(chatId: String) {
        if demo != nil {
            demoSessionRefs.removeAll { $0.chatId == chatId }
            return
        }
        workspace?.removeSessionRef(chatId: chatId)
    }

    // MARK: One-click invitations

    /// Production uses `comet://invite/{chatId}/{sessionId}/{grantId}`;
    /// staging uses the equivalent configured scheme. The session and grant
    /// segments route engine authority; this viewport only needs the chat id
    /// to pin membership and open the session.
    func openInvitation(url: URL) {
        guard let chatId = Self.invitationChatId(url) else { return }
        pendingInviteChatId = chatId
        drainPendingInvite()
    }

    private func drainPendingInvite() {
        guard phase == .ready, let chatId = pendingInviteChatId else { return }
        pendingInviteChatId = nil
        if chat(id: chatId) == nil {
            addSessionRef(chatId)
        }
        launchRoute = .chat(chatId)
    }

    /// Mirrors `comet_proto::CometInvitation::parse_deep_link`: exactly three
    /// non-empty `[A-Za-z0-9._-]{1,256}` segments, no query or fragment.
    static func invitationChatId(_ url: URL) -> String? {
        let prefix = "\(ReleaseConfig.inviteScheme)://invite/"
        guard url.absoluteString.hasPrefix(prefix) else { return nil }
        let path = String(url.absoluteString.dropFirst(prefix.count))
        guard !path.contains("?"), !path.contains("#") else { return nil }
        let segments = path.split(separator: "/", omittingEmptySubsequences: false)
        let valid = segments.count == 3 && segments.allSatisfy { segment in
            !segment.isEmpty && segment.count <= 256 && segment.allSatisfy {
                ($0.isASCII && ($0.isLetter || $0.isNumber)) || $0 == "-" || $0 == "_" || $0 == "."
            }
        }
        guard valid else { return nil }
        return String(segments[0])
    }


    func chats(in spaceId: String) -> [Chat] {
        if let demo { return demo.lists.activeBySpace[spaceId] ?? [] }
        return workspace?.chats(in: spaceId) ?? []
    }

    func settledChats(in spaceId: String) -> [Chat] {
        demo?.lists.archivedBySpace[spaceId] ?? workspace?.settledChats(in: spaceId) ?? []
    }

    func chat(id: String) -> Chat? {
        demo?.lists.chatsById[id] ?? workspace?.chat(id: id)
    }

    /// state.rs `space_for_chat` — nil for a dangling/missing space_id.
    func space(for chat: Chat) -> Space? {
        guard let spaceId = chat.spaceId else { return nil }
        return space(id: spaceId)
    }

    func activity(chatId: String, now: Int64) -> SessionActivity {
        let store = sessionStores[chatId]
        return sessionActivity(
            workspace: demo?.sessions[chatId] ?? workspace?.sessions[chatId],
            published: store?.publishedSession ?? cachedListMetadata(chatId: chatId)?.session,
            transcript: store?.transcriptActivity, now: now
        )
    }

    func hasPendingSend(chatId: String) -> Bool {
        !(sessionStores[chatId]?.pendingSends.isEmpty ?? true)
    }

    func indicator(for chat: Chat) -> ChatIndicator {
        let now = nowMs()
        return chatIndicator(chat: chat, live: activity(chatId: chat.id, now: now).status)
    }

    func spaceIndicator(_ spaceId: String) -> ChatIndicator? {
        let now = nowMs()
        var result: ChatIndicator?
        for chat in chats(in: spaceId) {
            let indicator = chatIndicator(chat: chat, live: activity(chatId: chat.id, now: now).status)
            if result == nil || indicator.rawValue < result!.rawValue { result = indicator }
        }
        return result
    }

    func deviceName(_ deviceId: String) -> String {
        (demo?.lists.devicesById[deviceId] ?? workspace?.device(id: deviceId))?.name ?? deviceId
    }

    func deviceOnline(_ deviceId: String) -> Bool {
        if let demo {
            guard let seen = demo.lists.devicesById[deviceId]?.lastSeenAt else { return false }
            return nowMs() - seen < presenceFreshMs
        }
        return workspace?.deviceOnline(deviceId) ?? false
    }

    func listHarnesses(space: Space) async throws -> [HarnessInfo] {
        if demo != nil {
            return HarnessCatalog.fallbackHarnesses + [HarnessInfo(id: "omp", label: "OMP")]
        }
        guard let workspace else { throw MobileSessionError.unavailable("Not connected") }
        return try await workspace.listHarnesses(deviceId: space.deviceId)
    }

    /// Only curated harnesses have an offline fallback. Dynamic catalogs
    /// propagate failure so the picker can explain it and offer retry.
    func listModelsDetailed(space: Space, harness: String) async throws -> [ModelInfo] {
        let fallback = HarnessCatalog.models(for: harness)
        if demo != nil {
            // Demo-only selectors keep simulated Scaffold launches usable;
            // real OMP catalogs always come from the selected host.
            if harness == "omp" {
                return [("codex", "openai-codex"), ("claude-code", "anthropic")].flatMap { harness, provider in
                    HarnessCatalog.models(for: harness).map {
                        ModelInfo(id: "\(provider)/\($0.id)", label: $0.label,
                                  description: $0.description, reasoningLevels: $0.reasoningLevels)
                    }
                }
            }
            return fallback
        }
        do {
            guard let workspace else { throw MobileSessionError.unavailable("Not connected") }
            let live = try await workspace.listModels(deviceId: space.deviceId, harness: harness)
            return live.isEmpty ? fallback : live
        } catch {
            guard !fallback.isEmpty else { throw error }
            return fallback
        }
    }

    /// Refs of the space's repo (git spaces only).
    func listRefs(space: Space) async -> [RepoRef]? {
        if let demo {
            try? await Task.sleep(nanoseconds: 120_000_000)
            return demo.listRefs(spacePath: space.path)
        }
        return await workspace?.listRefs(deviceId: space.deviceId, repoPath: space.path)
    }

    /// Draft-mode checkout switch: `git checkout` in the SPACE's folder.
    /// Returns an error message, or nil on success.
    func switchSpaceRef(space: Space, refName: String) async -> String? {
        if let demo {
            try? await Task.sleep(nanoseconds: 200_000_000)
            demo.switchRef(path: space.path, refName: refName)
            return nil
        }
        guard let workspace else { return "Not connected" }
        return await workspace.switchRef(deviceId: space.deviceId,
                                         repoPath: space.path, refName: refName)
    }

    /// Mid-session ref switch (desktop switch_session_ref): retarget onto the
    /// ref's existing worktree (row writes, no git), else checkout in the
    /// session's own cwd on the host. Returns an error message or nil.
    func switchSessionRef(chat: Chat, ref: RepoRef) async -> String? {
        guard let cwd = chat.cwd else { return "Session has no working folder" }
        if let worktree = ref.worktreePath {
            if worktree == cwd { return nil }  // already here
            if let demo {
                if let ix = demo.chats.firstIndex(where: { $0.id == chat.id }) {
                    demo.chats[ix].cwd = worktree
                    demo.chats[ix].branch = ref.name
                }
                return nil
            }
            workspace?.setChatCheckout(chatId: chat.id, cwd: worktree, branch: ref.name)
            return nil
        }
        if let demo {
            try? await Task.sleep(nanoseconds: 200_000_000)
            demo.switchRef(path: cwd, refName: ref.name)
            if let ix = demo.chats.firstIndex(where: { $0.id == chat.id }) {
                demo.chats[ix].branch = ref.name
            }
            return nil
        }
        guard let workspace else { return "Not connected" }
        let error = await workspace.switchRef(deviceId: chat.deviceId,
                                              repoPath: cwd, refName: ref.name)
        if error == nil {
            // The host's HEAD watcher reconciles chat.branch eventually;
            // stamp it optimistically so the UI answers immediately.
            workspace.setChatCheckout(chatId: chat.id, cwd: cwd, branch: ref.name)
        }
        return error
    }

    /// CreateWorktree off the base ref; returns the new worktree's path.
    func createWorktree(space: Space, base: String) async -> String? {
        if let demo {
            try? await Task.sleep(nanoseconds: 250_000_000)
            return demo.createWorktree(spacePath: space.path, base: base)
        }
        return await workspace?.createWorktree(deviceId: space.deviceId,
                                               repoPath: space.path, branch: base)
    }

    @discardableResult
    func createChat(space: Space, config chatConfig: ChatConfig,
                    branch: String? = nil, cwd: String? = nil) async throws -> String {
        if let demo {
            let id = "chat-\(UUID().uuidString.lowercased().prefix(8))"
            demo.chats.append(Chat(id: id, deviceId: space.deviceId, title: nil, archived: false,
                                   cwd: cwd ?? space.path, branch: branch, checkoutId: nil,
                                   config: chatConfig, lastMessagePreview: nil, lastMessageAt: nil,
                                   createdAt: nowMs(), harnessSessionId: nil,
                                   harnessSessionCwd: nil, spaceId: space.id, lastSeenAt: nowMs()))
            return id
        }
        guard let workspace else { throw MobileSessionError.unavailable("Not connected") }
        return try await workspace.createChat(space: space, config: chatConfig, branch: branch, cwd: cwd)
    }

    var launchesScaffoldSessions: Bool {
        demo != nil || config?.mode == .scaffold
    }

    func launchScaffoldSession(space: Space, prompt: String, harness: String,
                               model modelId: String, reasoning: String?,
                               databaseEnvironment: ScaffoldDatabaseEnvironment,
                               sourceRef: String?, creationId: String = UUID().uuidString.lowercased(),
                               images: [MobileImageAttachment] = []) async throws -> String {
        let selected = modelId.trimmingCharacters(in: .whitespacesAndNewlines)
        let provider: String
        let providerModel: String
        let persistedModel: String
        if let slash = selected.lastIndex(of: "/") {
            let prefix = selected[..<slash].lowercased()
            providerModel = String(selected[selected.index(after: slash)...])
            if prefix == "anthropic" {
                provider = "anthropic"
                persistedModel = "anthropic/\(providerModel)"
            } else if prefix == "openai" || prefix == "openai-codex" {
                provider = "openai"
                persistedModel = "openai-codex/\(providerModel)"
            } else {
                throw MobileSessionError.unavailable("Select a supported Scaffold model")
            }
        } else {
            provider = harness == "codex" ? "openai" : "anthropic"
            providerModel = selected
            persistedModel = harness == "codex"
                ? "openai-codex/\(selected)" : "anthropic/\(selected)"
        }
        let sourceRef = sourceRef?.trimmingCharacters(in: .whitespacesAndNewlines)
        let resolvedRef = sourceRef?.isEmpty == false ? sourceRef! : "master"
        let launch = ScaffoldLaunchConfig(
            provider: provider,
            providerModel: providerModel,
            persistedModel: persistedModel,
            reasoning: reasoning,
            databaseEnvironment: databaseEnvironment,
            sourceRef: resolvedRef
        )
        if let demo {
            let chatId = creationId
            let chatConfig = ChatConfig(harness: "omp", model: persistedModel,
                                        reasoning: reasoning, sandbox: "workspace-write")
            demo.chats.append(Chat(
                id: chatId, deviceId: space.deviceId, title: "Scaffold session", archived: false,
                cwd: ".", branch: resolvedRef, checkoutId: nil, config: chatConfig,
                lastMessagePreview: nil, lastMessageAt: nil, createdAt: nowMs(),
                harnessSessionId: nil, harnessSessionCwd: nil,
                spaceId: space.id, lastSeenAt: nowMs()
            ))
            let environment = SessionEnvironment(
                source: SessionEnvironmentSource(
                    kind: "scaffold", sandboxId: "demo-sandbox", region: nil,
                    lifecycle: "ready", lifecycleEpoch: 1, links: nil
                ),
                name: "Scaffold session", ownerPrincipal: "demo",
                scope: CollaborationScope(projectId: "demo", deploymentId: "demo", sessionId: chatId),
                sourceRef: resolvedRef, lastActivityAt: nowMs(),
                databaseEnvironment: databaseEnvironment
            )
            demoSessionRefs.insert(SessionRef(chatId: chatId, addedAt: nowMs(),
                                              environment: environment), at: 0)
            let store = demo.sessionStore(for: chatId)
            store.attachmentUploader = { [weak self] images in
                guard let self else { throw MobileSessionError.unavailable("Not connected") }
                return try await self.uploadImages(images, chatId: chatId)
            }
            guard await store.sendRun(prompt: prompt, chat: demo.chats.last, images: images) else {
                throw MobileSessionError.unavailable(store.sendFailure ?? "Could not send the message")
            }
            return chatId
        }
        guard let workspace else { throw MobileSessionError.unavailable("Not connected") }
        if scaffoldRoutes[creationId] == nil {
            scaffoldRoutes[creationId] = try await workspace.prepareScaffoldSession(
                space: space, chatId: creationId, launch: launch
            )
        }
        guard let chat = chat(id: creationId), let store = sessionStore(for: chat) else {
            throw MobileSessionError.unavailable("The created session is not available yet")
        }
        guard await store.sendRun(prompt: prompt, chat: chat, images: images) else {
            throw MobileSessionError.unavailable(store.sendFailure ?? "Could not send the message")
        }
        return creationId
    }

    func uploadImages(_ images: [MobileImageAttachment], chatId: String) async throws -> [String] {
        if demo != nil {
            let directory = FileManager.default.temporaryDirectory.appendingPathComponent("crew-demo-images", isDirectory: true)
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            return try images.map { image in
                let file = directory.appendingPathComponent("\(image.id.uuidString).\((image.filename as NSString).pathExtension)")
                try image.bytes.write(to: file, options: .atomic)
                return file.path
            }
        }
        guard let workspace else { throw MobileSessionError.unavailable("Not connected") }
        let deviceId: String
        if let environment = scaffoldEnvironment(chatId: chatId), environment.source.kind == "scaffold" {
            guard let controller = scaffoldRoutes[chatId]?.controllerDeviceId
                ?? workspace.chat(id: chatId)?.deviceId
                ?? scaffoldControllerDeviceId() else {
                throw MobileSessionError.unavailable("This session has no Scaffold controller")
            }
            let route = try await workspace.scaffoldRoute(controllerDeviceId: controller, environment: environment)
            scaffoldRoutes[chatId] = route
            deviceId = route.ownerDeviceId
        } else {
            guard let host = workspace.chat(id: chatId)?.deviceId
                ?? workspace.sessions[chatId]?.deviceId, !host.isEmpty else {
                throw MobileSessionError.unavailable("This session has no known desktop host")
            }
            deviceId = host
        }
        return try await workspace.uploadImages(images, chatId: chatId, deviceId: deviceId)
    }

    func forkSession(_ source: Chat) async throws -> String {
        if let demo {
            guard source.config != nil, source.harnessSessionId != nil else {
                throw MobileSessionError.unavailable("This session has no native context to fork")
            }
            var fork = source
            fork.id = "chat-\(UUID().uuidString.lowercased().prefix(8))"
            fork.title = source.title.map { "Fork of \($0)" }
            fork.lastMessagePreview = nil
            fork.lastMessageAt = nil
            fork.lastSeenAt = nil
            fork.createdAt = nowMs()
            fork.harnessSessionId = nil
            fork.harnessSessionCwd = nil
            demo.chats.append(fork)
            let sourceStore = demo.sessionStore(for: source.id)
            let targetStore = demo.sessionStore(for: fork.id)
            targetStore.setEntries(sourceStore.entries)
            return fork.id
        }
        guard let workspace else { throw MobileSessionError.unavailable("Not connected") }
        return try await workspace.forkSession(source: source)
    }

    /// Browse folders on a remote device (the desktop add-space palette's data
    /// path). Demo mode serves a canned tree; live mode asks the device over
    /// the relay.
    func listFolders(deviceId: String, path: String?) async -> FolderListing? {
        if let demo {
            try? await Task.sleep(nanoseconds: 120_000_000)  // feel like a network hop
            let target = path ?? demo.homePath(deviceId: deviceId)
            return demo.listFolders(deviceId: deviceId, path: target)
        }
        return await workspace?.listFolders(deviceId: deviceId, path: path)
    }

    @discardableResult
    func createSpace(deviceId: String, path: String, gitDetected: Bool = false) async -> String? {
        if let demo {
            if let existing = demo.spaces.first(where: { $0.deviceId == deviceId && $0.path == path }) {
                return existing.id
            }
            let id = "space-\(UUID().uuidString.lowercased().prefix(8))"
            demo.spaces.append(Space(id: id, deviceId: deviceId, path: path, name: nil,
                                     gitDetected: gitDetected, gitCheckedAt: nil, checkoutId: nil,
                                     createdAt: nowMs()))
            return id
        }
        return await workspace?.createSpace(deviceId: deviceId, path: path, gitDetected: gitDetected)
    }

    func archive(chatId: String) {
        if let demo {
            if let ix = demo.chats.firstIndex(where: { $0.id == chatId }) {
                demo.chats[ix].archived = true
            }
            return
        }
        workspace?.setArchived(chatId: chatId, archived: true)
    }

    func restoreChat(chatId: String) {
        if let demo {
            if let ix = demo.chats.firstIndex(where: { $0.id == chatId }) {
                demo.chats[ix].archived = false
            }
            return
        }
        workspace?.setArchived(chatId: chatId, archived: false)
    }

    func setChatConfig(chatId: String, config: ChatConfig) {
        if let demo {
            if let ix = demo.chats.firstIndex(where: { $0.id == chatId }) {
                demo.chats[ix].config = config
            }
            return
        }
        workspace?.setChatConfig(chatId: chatId, config: config)
    }

    func markSeen(chatId: String) {
        if let demo {
            if let ix = demo.chats.firstIndex(where: { $0.id == chatId }) {
                demo.chats[ix].lastSeenAt = nowMs()
            }
            return
        }
        workspace?.markSeen(chatId: chatId)
    }

    /// Persist every open doc now (app backgrounding).
    func flushDocs() {
        workspace?.flushToDisk()
        sessionStores.values.forEach { $0.flushToDisk() }
    }

    /// Diagnostics access (live e2e probe).
    var diagnosticsConfig: AppConfig? { config }

    // MARK: Session stores

    func sessionStore(for chat: Chat) -> SessionStore? {
        let environment = scaffoldEnvironment(chatId: chat.id)
        return sessionStore(chatId: chat.id,
                            deploymentId: environment?.scope.deploymentId,
                            environment: environment)
    }

    func sessionStore(for sessionRef: SessionRef) -> SessionStore? {
        let environment = sessionRef.environment ?? scaffoldEnvironment(chatId: sessionRef.chatId)
        return sessionStore(chatId: sessionRef.chatId,
                            deploymentId: environment?.scope.deploymentId,
                            environment: environment)
    }

    private func sessionStore(chatId: String, deploymentId: String?,
                              environment: SessionEnvironment?, metadataOnly: Bool = false) -> SessionStore? {
        if let demo {
            let store = demo.sessionStore(for: chatId)
            store.attachmentUploader = { [weak self] images in
                guard let self else { throw MobileSessionError.unavailable("Not connected") }
                return try await self.uploadImages(images, chatId: chatId)
            }
            return store
        }
        guard let config else { return nil }
        let store: SessionStore
        if let existing = sessionStores[chatId] {
            store = existing
            existing.updateDeploymentId(deploymentId)
            if !metadataOnly {
                metadataOnlyStores.remove(chatId)
                if pendingMetadataReleases.remove(chatId) != nil { scheduleMetadataRelease() }
                existing.activateTranscript()
            }
        } else {
            store = SessionStore(chatId: chatId, config: config, deploymentId: deploymentId,
                                 metadataOnly: metadataOnly)
            sessionStores[chatId] = store
            if metadataOnly { metadataOnlyStores.insert(chatId) }
            store.start()
        }
        configureSessionTransport(store: store, environment: environment)
        if !metadataOnly { retainTranscriptBuilder(chatId: chatId) }
        return store
    }

    private func retainTranscriptBuilder(chatId: String) {
        recentTranscriptIds.removeAll { $0 == chatId }
        recentTranscriptIds.append(chatId)
        while recentTranscriptIds.count > 3 {
            guard let index = recentTranscriptIds.firstIndex(where: {
                $0 != chatId && $0 != notifications.visibleChatId
            }) else { break }
            let oldest = recentTranscriptIds.remove(at: index)
            sessionStores[oldest]?.transcriptBuilder.reset()
        }
    }

    private func scaffoldEnvironment(chatId: String) -> SessionEnvironment? {
        scaffoldRoutes[chatId]?.environment
            ?? workspace?.sessionRef(id: chatId)?.environment
    }

    private func scaffoldControllerDeviceId() -> String? {
        guard let workspace else { return nil }
        return workspace.devices.first(where: {
            $0.platform != "ios" && workspace.deviceOnline($0.id)
        })?.id ?? workspace.devices.first(where: { $0.platform != "ios" })?.id
    }

    private func configureSessionTransport(store: SessionStore,
                                           environment: SessionEnvironment?) {
        let chatId = store.chatId
        store.attachmentUploader = { [weak self] images in
            guard let self else { throw MobileSessionError.unavailable("Not connected") }
            return try await self.uploadImages(images, chatId: chatId)
        }
        store.commandSender = { [weak self] payload in
            guard let self, let workspace = self.workspace else {
                throw MobileSessionError.unavailable("Not connected")
            }
            // Resolve from current workspace state, not the route captured when
            // a cached transcript was first opened.
            let environment = self.scaffoldEnvironment(chatId: chatId) ?? environment
            if let environment, environment.source.kind == "scaffold" {
                guard let controllerDeviceId = self.scaffoldRoutes[chatId]?.controllerDeviceId
                    ?? workspace.chat(id: chatId)?.deviceId
                    ?? self.scaffoldControllerDeviceId() else {
                    throw MobileSessionError.unavailable("This session has no Scaffold controller")
                }
                try await workspace.sendScaffoldCommand(
                    controllerDeviceId: controllerDeviceId,
                    environment: environment, payload: payload
                )
            } else {
                try await workspace.sendSessionCommand(chatId: chatId, payload: payload)
            }
        }
    }

    /// Reading a list label never creates a room or hydrates a transcript.
    /// Environment names are the canonical Scaffold labels used by desktop;
    /// ordinary workspace titles remain authoritative for local renames.
    /// An explicit fallback bypasses transcript-derived previews for notifications.
    func sessionTitle(for chat: Chat, fallbackTitle: String? = nil) -> String {
        let store = demo?.sessionStore(for: chat.id) ?? sessionStores[chat.id]
        return normalizedSessionTitle(workspace?.sessionRef(id: chat.id)?.environment?.name)
            ?? normalizedSessionTitle(scaffoldRoutes[chat.id]?.environment.name)
            ?? normalizedSessionTitle(store?.publishedEnvironment?.name)
            ?? normalizedSessionTitle(cachedListMetadata(chatId: chat.id)?.environment?.name)
            ?? normalizedSessionTitle(chat.title)
            ?? fallbackTitle
            ?? store?.previewTitle
            ?? cachedListMetadata(chatId: chat.id)?.previewTitle
            ?? chat.displayTitle
    }

    func sessionTitle(for sessionRef: SessionRef) -> String {
        let store = demo?.sessionStore(for: sessionRef.chatId) ?? sessionStores[sessionRef.chatId]
        return normalizedSessionTitle(sessionRef.environment?.name)
            ?? normalizedSessionTitle(scaffoldRoutes[sessionRef.chatId]?.environment.name)
            ?? normalizedSessionTitle(store?.publishedEnvironment?.name)
            ?? normalizedSessionTitle(cachedListMetadata(chatId: sessionRef.chatId)?.environment?.name)
            ?? store?.previewTitle
            ?? cachedListMetadata(chatId: sessionRef.chatId)?.previewTitle
            ?? sessionRef.fallbackTitle
    }

    func releaseSessionStore(chatId: String) {
        // Opened transcript stores stay warm; row metadata has a separate lease.
    }

    /// Sweep removed memberships and refresh only rows actually on screen.
    /// Empty bootstrap projections and stores serving a send/navigation stay safe.
    func preloadSessionMetadata() {
        let visible = workspace?.lists.memberIds ?? []
        // An empty projection can be bootstrap/recovery, not a membership
        // revocation. Never invalidate a store still used by an open view or
        // an in-flight launch/send; authoritative room access stays server-side.
        if !visible.isEmpty {
            for id in Array(sessionStores.keys) where !visible.contains(id) {
                guard id != notifications.visibleChatId,
                      scaffoldRoutes[id] == nil,
                      let store = sessionStores[id], !store.sending,
                      store.pendingSends.isEmpty else { continue }
                sessionStores.removeValue(forKey: id)?.stop()
                metadataOnlyStores.remove(id)
                recentTranscriptIds.removeAll { $0 == id }
                listMetadata.removeValue(forKey: id)
            }
        }
        for id in metadataReaders.keys where visible.isEmpty || visible.contains(id) {
            loadVisibleMetadata(chatId: id)
        }
        if !visible.isEmpty {
            for id in Array(listMetadata.keys) where !visible.contains(id) {
                listMetadata.removeValue(forKey: id)
            }
        }
    }

    func retainListMetadata(chatId: String) {
        guard demo == nil else { return }
        metadataReaders[chatId, default: 0] += 1
        pendingMetadataReleases.remove(chatId)
        scheduleMetadataRelease()
        if metadataReaders[chatId] == 1 { loadVisibleMetadata(chatId: chatId) }
    }

    private func loadVisibleMetadata(chatId: String) {
        let environment = scaffoldEnvironment(chatId: chatId)
        _ = sessionStore(chatId: chatId, deploymentId: environment?.scope.deploymentId,
                         environment: environment, metadataOnly: true)
    }

    private func cachedListMetadata(chatId: String) -> ListMetadata? {
        guard let cached = listMetadata[chatId],
              cached.deploymentId == scaffoldEnvironment(chatId: chatId)?.scope.deploymentId else { return nil }
        return cached
    }

    func releaseListMetadata(chatId: String) {
        guard let readers = metadataReaders[chatId] else { return }
        if readers > 1 {
            metadataReaders[chatId] = readers - 1
            scheduleMetadataRelease()
            return
        }
        metadataReaders.removeValue(forKey: chatId)
        if metadataOnlyStores.contains(chatId) { pendingMetadataReleases.insert(chatId) }
        scheduleMetadataRelease()
    }

    /// Wait for viewport movement to settle before synchronously persisting a
    /// metadata document. One sleeper handles the whole viewport, not each row.
    private func scheduleMetadataRelease() {
        guard !pendingMetadataReleases.isEmpty else {
            metadataReleaseTask?.cancel()
            metadataReleaseTask = nil
            metadataReleaseDeadline = nil
            return
        }
        metadataReleaseDeadline = DispatchTime.now().uptimeNanoseconds + 300_000_000
        guard metadataReleaseTask == nil else { return }
        metadataReleaseTask = Task { @MainActor [weak self] in
            while !Task.isCancelled, let deadline = self?.metadataReleaseDeadline {
                let now = DispatchTime.now().uptimeNanoseconds
                if now < deadline {
                    do { try await Task.sleep(nanoseconds: deadline - now) }
                    catch { return }
                    continue
                }
                guard let id = self?.pendingMetadataReleases.first else {
                    self?.metadataReleaseDeadline = nil
                    self?.metadataReleaseTask = nil
                    return
                }
                self?.pendingMetadataReleases.remove(id)
                self?.releaseInactiveMetadata(chatId: id)
                // Never export several dirty documents in a single UI turn.
                await Task.yield()
            }
        }
    }

    private func releaseInactiveMetadata(chatId: String) {
        guard metadataReaders[chatId] == nil else { return }
        guard metadataOnlyStores.contains(chatId),
              chatId != notifications.visibleChatId, scaffoldRoutes[chatId] == nil,
              let store = sessionStores[chatId], !store.sending,
              store.pendingSends.isEmpty else { return }
        // Retain labels and timestamped activity, not a document/socket per row.
        // sessionActivity still expires stale publications while off screen.
        let previous = cachedListMetadata(chatId: chatId)
        listMetadata[chatId] = ListMetadata(deploymentId: store.deploymentId,
                                            environment: store.publishedEnvironment ?? previous?.environment,
                                            previewTitle: store.previewTitle ?? previous?.previewTitle,
                                            session: store.publishedSession ?? previous?.session)
        metadataOnlyStores.remove(chatId)
        sessionStores.removeValue(forKey: chatId)
        store.stop()
    }
}

extension AppModel {
    /// Exercise the production cache sweep without restoring credentials,
    /// starting rooms, or signing out the app's shared notification service.
    static func runStoreEvictionRegression() async -> Bool {
        let probe = AppModel()
        let config = AppConfig(edgeURL: URL(string: "http://127.0.0.1:1")!, mode: .dev,
                               userId: "eviction-\(UUID().uuidString)", projectScope: "eviction",
                               deviceId: "viewer", deviceName: "Crew regression")
        let workspace = WorkspaceStore(config: config)
        probe.workspace = workspace
        // Leave probe.config unset: metadata warming cannot open replacement
        // rooms, including when a broken sweep removes one of these fixtures.
        let member = SessionStore(chatId: "eviction-member", config: config, offline: true)
        let idle = SessionStore(chatId: "eviction-idle", config: config, offline: true)
        let visible = SessionStore(chatId: "eviction-visible", config: config, offline: true)
        let routed = SessionStore(chatId: "eviction-routed", config: config, offline: true)
        let sending = SessionStore(chatId: "eviction-sending", config: config, offline: true)
        let queued = SessionStore(chatId: "eviction-queued", config: config, offline: true)
        let stores = [member, idle, visible, routed, sending, queued]
        probe.sessionStores = Dictionary(uniqueKeysWithValues: stores.map { ($0.chatId, $0) })
        let oldVisibleChatId = probe.notifications.visibleChatId
        defer {
            probe.notifications.visibleChatId = oldVisibleChatId
            for store in stores { store.stop() }
        }

        probe.notifications.visibleChatId = nil
        probe.preloadSessionMetadata()
        probe.notifications.visibleChatId = oldVisibleChatId
        guard stores.allSatisfy({ probe.sessionStores[$0.chatId] === $0 }) else {
            E2ERunner.log("FAIL Crew store eviction: empty membership replaced or evicted a cached store")
            return false
        }
        guard workspace.addSessionRef(chatId: member.chatId) != nil,
              workspace.sessionRefs.map(\.chatId) == [member.chatId] else {
            E2ERunner.log("FAIL Crew store eviction: member fixture did not project")
            return false
        }
        let environment = SessionEnvironment(
            source: SessionEnvironmentSource(kind: "scaffold", sandboxId: "eviction-sandbox"),
            ownerPrincipal: config.userId,
            scope: CollaborationScope(projectId: config.projectScope,
                                      deploymentId: "eviction-deployment", sessionId: routed.chatId))
        probe.scaffoldRoutes[routed.chatId] = ScaffoldControlRoute(
            controllerDeviceId: "controller", ownerDeviceId: "owner", actorSubject: config.userId,
            grantId: "eviction-grant",
            projection: SessionRoomProjection(projectId: config.projectScope,
                                              deploymentId: "eviction-deployment", sessionId: routed.chatId),
            environment: environment)
        let queuedMessageId = queued.stagePendingSend(prompt: "queued fixture")
        defer { queued.dropPendingSend(messageId: queuedMessageId) }

        // Suspend a real send during attachment upload, before its optimistic
        // echo is staged. Only `sending`, not `pendingSends`, can protect it.
        var upload: CheckedContinuation<[String], Error>?
        var started: CheckedContinuation<Void, Never>?
        var sendTask: Task<Bool, Never>?
        sending.attachmentUploader = { _ in
            try await withCheckedThrowingContinuation { continuation in
                upload = continuation
                let waiting = started
                started = nil
                waiting?.resume()
            }
        }
        let image = MobileImageAttachment(id: UUID(), filename: "eviction.png",
                                          bytes: Data(), preview: UIImage())
        await withCheckedContinuation { ready in
            started = ready
            sendTask = Task {
                let result = await sending.sendRun(prompt: "suspended fixture", chat: nil, images: [image])
                // A premature send failure must report a failure, not leave
                // the regression waiting for an upload that never started.
                let waiting = started
                started = nil
                waiting?.resume()
                return result
            }
        }

        // There are no awaits while shared notification visibility is changed.
        probe.notifications.visibleChatId = visible.chatId
        let suspended = upload != nil && sending.sending && sending.pendingSends.isEmpty
        probe.preloadSessionMetadata()
        let activeRetained = [member, visible, routed, sending, queued].allSatisfy { (store: SessionStore) in
            probe.sessionStores[store.chatId] === store
        }
        let idleEvicted = probe.sessionStores[idle.chatId] == nil
        probe.notifications.visibleChatId = oldVisibleChatId

        // Always finish the upload and join the send before checking results,
        // including when the production sweep has evicted the sending store.
        upload?.resume(returning: ["/tmp/eviction.png"])
        upload = nil
        let sent = await sendTask?.value ?? false
        guard suspended, sent, !sending.sending, sending.pendingSends.isEmpty,
              !queued.sending, queued.pendingSends.map(\.messageId) == [queuedMessageId],
              activeRetained, idleEvicted else {
            E2ERunner.log("FAIL Crew store eviction: active identity, idle pruning, or suspended send completion")
            return false
        }

        // Protection is temporary: after each reason goes away, the same
        // nonmembers must be evictable while legitimate membership survives.
        probe.notifications.visibleChatId = nil
        probe.scaffoldRoutes.removeAll()
        queued.dropPendingSend(messageId: queuedMessageId)
        probe.preloadSessionMetadata()
        guard probe.sessionStores.count == 1,
              probe.sessionStores[member.chatId] === member else {
            E2ERunner.log("FAIL Crew store eviction: inactive nonmembers leaked or legitimate member was evicted")
            return false
        }
        return true
    }
}
