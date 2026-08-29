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
    private var config: AppConfig?

    // Persisted connection settings.
    @ObservationIgnored @AppStorage("edgeURL") var edgeURLString = "https://comet.internal.ashler.com"
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
        if demo != nil { return }
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
        workspace?.stop()
        workspace = nil
        sessionStores.values.forEach { $0.stop() }
        sessionStores.removeAll()
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
        let store = WorkspaceStore(config: config)
        workspace = store
        store.start()
        phase = .ready
        drainPendingInvite()
    }

    // MARK: Unified data accessors (demo or live — one path for views)

    var spaces: [Space] { demo?.spaces ?? workspace?.spaces ?? [] }

    var connected: Bool { demo != nil || workspace?.connected == true }

    var overviewChats: [Chat] {
        if let demo {
            let liveIds = Set(demo.spaces.map(\.id))
            let live = demo.chats.filter { !$0.archived && $0.spaceId.map(liveIds.contains) == true }
            return sortActive(live)
        }
        return workspace?.overviewChats ?? []
    }
    /// Imported memberships with no chat row — a chat row always wins and
    /// renders as a normal session row with full context.
    var sharedSessionRefs: [SessionRef] {
        if let demo {
            let rowIds = Set(demo.chats.map(\.id))
            return demoSessionRefs.filter { !rowIds.contains($0.chatId) }
        }
        return workspace?.sharedSessionRefs ?? []
    }

    func sessionRef(id: String) -> SessionRef? {
        sharedSessionRefs.first { $0.chatId == id }
    }

    @discardableResult
    func addSessionRef(_ rawChatId: String) -> SessionRef? {
        guard let uuid = UUID(uuidString: rawChatId.trimmingCharacters(in: .whitespacesAndNewlines))
        else { return nil }
        let chatId = uuid.uuidString.lowercased()
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

    /// `comet://invite/{chatId}/{sessionId}/{grantId}` — the desktop's
    /// one-click join link (`CometInvitation`). The session/grant ids route
    /// engine command authority; a viewport needs only the chat id: pin
    /// membership when the workspace has no chat row, then open the session.
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
        let prefix = "comet://invite/"
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
        if let demo {
            return sortActive(demo.chats.filter { !$0.archived && $0.spaceId == spaceId })
        }
        return workspace?.chats(in: spaceId) ?? []
    }

    func chat(id: String) -> Chat? {
        (demo?.chats ?? workspace?.chats)?.first { $0.id == id }
    }

    /// state.rs `space_for_chat` — nil for a dangling/missing space_id.
    func space(for chat: Chat) -> Space? {
        guard let spaceId = chat.spaceId else { return nil }
        return spaces.first { $0.id == spaceId }
    }

    func indicator(for chat: Chat) -> ChatIndicator {
        if let demo {
            return chatIndicator(chat: chat, live: effectiveStatus(demo.sessions[chat.id], now: nowMs()))
        }
        return workspace?.indicator(for: chat) ?? .idle
    }

    func spaceIndicator(_ spaceId: String) -> ChatIndicator? {
        chats(in: spaceId).map { indicator(for: $0) }.min { $0.rawValue < $1.rawValue }
    }

    func deviceName(_ deviceId: String) -> String {
        (demo?.devices ?? workspace?.devices)?.first { $0.id == deviceId }?.name ?? deviceId
    }

    func deviceOnline(_ deviceId: String) -> Bool {
        if let demo {
            guard let seen = demo.devices.first(where: { $0.id == deviceId })?.lastSeenAt else { return false }
            return nowMs() - seen < presenceFreshMs
        }
        return workspace?.deviceOnline(deviceId) ?? false
    }

    /// Live model catalog from the space's owning device (the desktop's
    /// "catalog source = the device that runs the session" rule); static
    /// fallback when the device is unreachable.
    func listModels(space: Space, harness: String) async -> [ModelInfo] {
        if demo != nil {
            try? await Task.sleep(nanoseconds: 100_000_000)
            return HarnessCatalog.models(for: harness)
        }
        if let live = await workspace?.listModels(deviceId: space.deviceId, harness: harness),
           !live.isEmpty {
            return live
        }
        return HarnessCatalog.models(for: harness)
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
                    branch: String? = nil, cwd: String? = nil) -> String? {
        if let demo {
            let id = "chat-\(UUID().uuidString.lowercased().prefix(8))"
            demo.chats.append(Chat(id: id, deviceId: space.deviceId, title: nil, archived: false,
                                   cwd: cwd ?? space.path, branch: branch, checkoutId: nil,
                                   config: chatConfig, lastMessagePreview: nil, lastMessageAt: nil,
                                   createdAt: nowMs(), harnessSessionId: nil,
                                   harnessSessionCwd: nil, spaceId: space.id, lastSeenAt: nowMs()))
            return id
        }
        return workspace?.createChat(space: space, config: chatConfig, branch: branch, cwd: cwd)
    }

    var launchesScaffoldSessions: Bool {
        demo != nil || config?.mode == .scaffold
    }

    func launchScaffoldSession(space: Space, prompt: String, harness: String,
                               model modelId: String, reasoning: String?,
                               databaseEnvironment: ScaffoldDatabaseEnvironment,
                               sourceRef: String?) async throws -> String {
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
            let chatId = UUID().uuidString.lowercased()
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
            demo.sessionStore(for: chatId).sendRun(prompt: prompt, chat: demo.chats.last)
            return chatId
        }
        guard let workspace else { throw MobileSessionError.unavailable("Not connected") }
        let (chatId, route) = try await workspace.launchScaffoldSession(
            space: space, prompt: prompt, launch: launch
        )
        scaffoldRoutes[chatId] = route
        return chatId
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
        return sessionStore(chatId: chat.id, hostDeviceId: chat.deviceId,
                            deploymentId: environment?.scope.deploymentId,
                            scaffoldEnvironment: environment,
                            controllerDeviceId: chat.deviceId)
    }

    func sessionStore(for sessionRef: SessionRef) -> SessionStore? {
        let environment = sessionRef.environment ?? scaffoldEnvironment(chatId: sessionRef.chatId)
        return sessionStore(chatId: sessionRef.chatId, hostDeviceId: nil,
                            deploymentId: environment?.scope.deploymentId,
                            scaffoldEnvironment: environment,
                            controllerDeviceId: scaffoldControllerDeviceId())
    }

    private func sessionStore(chatId: String, hostDeviceId: String?,
                              deploymentId: String?,
                              scaffoldEnvironment: SessionEnvironment?,
                              controllerDeviceId: String?) -> SessionStore? {
        if let demo { return demo.sessionStore(for: chatId) }
        guard let config else { return nil }
        let store: SessionStore
        if let existing = sessionStores[chatId] {
            store = existing
            if existing.hostDeviceId != hostDeviceId { existing.hostDeviceId = hostDeviceId }
            existing.updateDeploymentId(deploymentId)
        } else {
            store = SessionStore(chatId: chatId, config: config, deploymentId: deploymentId)
            store.hostDeviceId = hostDeviceId
            sessionStores[chatId] = store
            store.start()
        }
        configureScaffoldTransport(store: store, environment: scaffoldEnvironment,
                                   controllerDeviceId: controllerDeviceId)
        return store
    }

    private func scaffoldEnvironment(chatId: String) -> SessionEnvironment? {
        scaffoldRoutes[chatId]?.environment
            ?? workspace?.sessionRefs.first(where: { $0.chatId == chatId })?.environment
    }

    private func scaffoldControllerDeviceId() -> String? {
        guard let workspace else { return nil }
        return workspace.devices.first(where: {
            $0.platform != "ios" && workspace.deviceOnline($0.id)
        })?.id ?? workspace.devices.first(where: { $0.platform != "ios" })?.id
    }

    private func configureScaffoldTransport(store: SessionStore,
                                            environment: SessionEnvironment?,
                                            controllerDeviceId: String?) {
        guard let workspace, let environment,
              environment.source.kind == "scaffold",
              let controllerDeviceId else {
            store.scaffoldCommandSender = nil
            return
        }
        store.scaffoldCommandSender = { [weak store] payload in
            Task { @MainActor in
                do {
                    try await workspace.sendScaffoldCommand(
                        controllerDeviceId: controllerDeviceId,
                        environment: environment,
                        payload: payload
                    )
                } catch {
                    store?.reportSendFailure(error.localizedDescription,
                                             messageId: payload.messageId)
                }
            }
        }
    }

    func sessionTitle(for sessionRef: SessionRef) -> String {
        guard let store = sessionStore(for: sessionRef),
              let entry = store.entries.first(where: { $0.role == .user }) else {
            return sessionRef.fallbackTitle
        }
        let text = entry.parts.compactMap { part -> String? in
            guard case .text(_, let text) = part else { return nil }
            return text
        }.joined(separator: " ")
        let oneLine = text.split(whereSeparator: \.isWhitespace).joined(separator: " ")
        guard !oneLine.isEmpty else { return sessionRef.fallbackTitle }
        return oneLine.count > 48 ? String(oneLine.prefix(48)) + "\u{2026}" : oneLine
    }

    func releaseSessionStore(chatId: String) {
        // Preloaded stores stay warm — nothing to evict on navigation.
    }

    /// Warm every non-archived session: stores hydrate from disk instantly
    /// and keep their rooms syncing, so opening a session never shows a
    /// loading state.
    func preloadSessions() {
        for chat in overviewChats {
            _ = sessionStore(for: chat)
        }
        for sessionRef in sharedSessionRefs {
            _ = sessionStore(for: sessionRef)
        }
    }
}
