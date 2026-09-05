// Session screen — transcript + status strip + composer (or question panel
// while input is requested, replacing the composer like the desktop). Reading
// marks the chat seen (the synced LWW marker behind the green dot everywhere).

import SwiftUI

struct SessionView: View {
    @Environment(AppModel.self) private var model
    let chatId: String
    @State private var showConfig = false
    @State private var refs: [RepoRef] = []
    @State private var catalogs: [String: [ModelInfo]] = [:]
    @State private var forking = false
    @State private var forkError: String?

    /// Width the nav bar's own controls need either side of the title — the
    /// back button leading, breathing room trailing.
    private static let headerChromeInset: CGFloat = 132

    /// The view's own width, the only reliable basis for capping the principal
    /// toolbar item (its container proposes an unbounded width).
    @State private var viewWidth: CGFloat = 0

    private var chat: Chat? { model.chat(id: chatId) }
    private var sessionRef: SessionRef? { model.sessionRef(id: chatId) }

    private var store: SessionStore? {
        if let chat { return model.sessionStore(for: chat) }
        if let sessionRef { return model.sessionStore(for: sessionRef) }
        return nil
    }

    private var displayTitle: String {
        if let chat { return chat.displayTitle }
        if let sessionRef { return model.sessionTitle(for: sessionRef) }
        return "Session"
    }


    private var chatSpace: Space? {
        guard let spaceId = chat?.spaceId else { return nil }
        return model.spaces.first { $0.id == spaceId }
    }

    var body: some View {
        Group {
            if (chat != nil || sessionRef != nil), let store {
                content(chat: chat, store: store)
                    .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { viewWidth = $0 }
            } else {
                VStack(spacing: 12) {
                    CometPulse()
                    Text("Opening session\u{2026}")
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.textFaint)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Theme.bg)
            }
        }
        .navigationTitle(displayTitle)  // feeds the back menu
        .navigationBarTitleDisplayMode(.inline)
        .toolbarBackground(.hidden, for: .navigationBar)
        .toolbar {
            if let chat {
                ToolbarItem(placement: .principal) {
                    // Tapping the header reconfigures model/effort mid-chat
                    // (the old app's header model pill); harness stays locked.
                    Button {
                        showConfig = true
                    } label: {
                        VStack(spacing: 1) {
                            HStack(spacing: 6) {
                                HarnessBadge(harness: chat.config?.harness ?? "claude-code", size: 12)
                                // The badge and chevron are fixed; only the
                                // title gives way, so a long name truncates
                                // instead of pushing the chevron off-screen.
                                Text(chat.displayTitle)
                                    .font(Theme.sans(13, weight: .medium))
                                    .foregroundStyle(Theme.text)
                                    .lineLimit(1)
                                    .truncationMode(.tail)
                                    .layoutPriority(1)
                                Image(systemName: "chevron.down")
                                    .font(.system(size: 8, weight: .semibold))
                                    .foregroundStyle(Theme.textFaint)
                                    .layoutPriority(2)
                            }
                            if let subtitle {
                                // Middle-truncated: the tail (device) identifies
                                // the session as much as the leading repo does.
                                Text(subtitle)
                                    .font(Theme.sans(10.5))
                                    .foregroundStyle(Theme.textMuted.opacity(0.6))
                                    .lineLimit(1)
                                    .truncationMode(.middle)
                            }
                        }
                        // A principal toolbar item is handed its IDEAL width, so
                        // an unconstrained header just runs past the bar and off
                        // the screen. Cap it to the centre region — the back
                        // button and any trailing item own the rest.
                        // A principal toolbar item is handed its IDEAL width, so
                        // an unconstrained header runs past the bar and off the
                        // screen. Cap it against the view's own width, leaving
                        // the back button and trailing padding their room.
                        .frame(maxWidth: max(140, viewWidth - Self.headerChromeInset))
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                }
            } else if sessionRef != nil {
                ToolbarItem(placement: .principal) {
                    HStack(spacing: 6) {
                        Image(systemName: "globe")
                            .font(.system(size: 11, weight: .medium))
                            .foregroundStyle(Theme.textMuted)
                        Text(displayTitle)
                            .font(Theme.sans(13, weight: .medium))
                            .foregroundStyle(Theme.text)
                            .lineLimit(1)
                    }
                    .frame(maxWidth: max(140, viewWidth - Self.headerChromeInset))
                }
            }
            if let chat, canFork(chat) {
                ToolbarItem(placement: .topBarTrailing) {
                    Button {
                        fork(chat)
                    } label: {
                        if forking {
                            ProgressView().controlSize(.mini)
                        } else {
                            Image(systemName: "rectangle.stack.badge.plus")
                        }
                    }
                    .disabled(forking)
                    .accessibilityLabel("Fork session")
                }
            }
        }
        .alert("Couldn’t fork session", isPresented: Binding(
            get: { forkError != nil },
            set: { if !$0 { forkError = nil } }
        )) {
            Button("OK", role: .cancel) { forkError = nil }
        } message: {
            Text(forkError ?? "Unknown error")
        }
        .alert("Couldn’t send", isPresented: Binding(
            get: { store?.sendFailure != nil },
            set: { if !$0 { store?.clearSendFailure() } }
        )) {
            Button("OK", role: .cancel) { store?.clearSendFailure() }
        } message: {
            Text(store?.sendFailure ?? "Unknown error")
        }
        .sheet(isPresented: $showConfig) {
            if let chat {
                let harness = chat.config?.harness ?? "claude-code"
                ModelPickerSheet(
                    harness: .constant(harness),
                    modelId: Binding(
                        get: {
                            chat.config?.model
                                ?? HarnessCatalog.defaultModel(for: harness).id
                        },
                        set: { newModel in
                            writeConfig(model: newModel, reasoning: chat.config?.reasoning)
                        }
                    ),
                    reasoning: Binding(
                        get: { chat.config?.reasoning },
                        set: { newReasoning in
                            writeConfig(model: chat.config?.model, reasoning: newReasoning)
                        }
                    ),
                    lockedHarness: true,
                    catalogs: catalogs,
                    checkout: checkoutContext(chat: chat)
                )
            }
        }
        .task(id: chatId) {
            guard let space = chatSpace else { return }
            let harness = chat?.config?.harness ?? "claude-code"
            catalogs[harness] = await model.listModels(space: space, harness: harness)
            guard space.gitDetected else { return }
            if let loaded = await model.listRefs(space: space) {
                refs = loaded
            }
        }
        .onAppear {
            if chat != nil {
                model.markSeen(chatId: chatId)
            }
            if model.launchSheet == "config", chat != nil {
                model.launchSheet = nil
                showConfig = true
            }
        }
        .onDisappear {
            if chat != nil {
                model.markSeen(chatId: chatId)
            }
            model.releaseSessionStore(chatId: chatId)
        }
    }

    private func canFork(_ chat: Chat) -> Bool {
        chat.config != nil && chat.harnessSessionId?.isEmpty == false
    }

    private func fork(_ chat: Chat) {
        guard !forking else { return }
        forking = true
        Task { @MainActor in
            defer { forking = false }
            do {
                let chatId = try await model.forkSession(chat)
                model.launchRoute = .chat(chatId)
            } catch {
                forkError = error.localizedDescription
            }
        }
    }

    /// Live-chat checkout context (git spaces only): read-only kind + the
    /// switchable ref list.
    private func checkoutContext(chat: Chat) -> SessionCheckoutContext? {
        guard let space = chatSpace, space.gitDetected, let cwd = chat.cwd else { return nil }
        return SessionCheckoutContext(
            isWorktree: cwd != space.path,
            cwd: cwd,
            refs: refs,
            currentBranch: chat.branch,
            onPick: { ref in
                let error = await model.switchSessionRef(chat: chat, ref: ref)
                if error == nil, let reloaded = await model.listRefs(space: space) {
                    refs = reloaded
                }
                return error
            }
        )
    }

    /// Merge a model/effort change into the chat's config row (LWW; the host
    /// picks it up on the next run dispatch).
    private func writeConfig(model newModel: String?, reasoning newReasoning: String?) {
        guard let chat else { return }
        var config = chat.config ?? ChatConfig(harness: "claude-code", model: nil,
                                               reasoning: nil, sandbox: "workspace-write")
        config.model = newModel
        config.reasoning = newReasoning
        model.setChatConfig(chatId: chat.id, config: config)
    }

    private var subtitle: String? {
        guard let chat else { return nil }
        var parts: [String] = []
        if let cwd = chat.cwd { parts.append((cwd as NSString).lastPathComponent) }
        if let branch = chat.branch, !branch.isEmpty { parts.append(branch) }
        parts.append(model.deviceName(chat.deviceId))
        return parts.joined(separator: " · ")
    }

    private func content(chat: Chat?, store: SessionStore) -> some View {
        TimelineView(.periodic(from: .now, by: 1)) { timeline in
            let now = Int64(timeline.date.timeIntervalSince1970 * 1000)
            let status = liveStatus(chatId: chatId, now: now)
            VStack(spacing: 0) {
                // The status strip floats over the transcript's faded bottom edge
                // instead of stacking below it — the loader sits on the
                // transparent zone and content is never pushed around.
                TranscriptView(store: store, chatId: chatId)
                    .overlay(alignment: .bottom) {
                        statusStrip(chatId: chatId, status: status, now: now)
                            .allowsHitTesting(false)
                    }

                if let request = store.openInputRequest {
                    QuestionPanel(requestId: request.requestId, questions: request.questions) { requestId, answers in
                        store.respondInput(requestId: requestId, answers: answers)
                    }
                    .padding(.bottom, 8)
                } else {
                    ComposerView(store: store, chat: chat, runLive: status == .working)
                        .padding(.bottom, 8)
                }
            }
            .background(Theme.bg.ignoresSafeArea())
            .motionAnimation(Motion.fadeQuick, value: store.openInputRequest?.requestId)
        }
    }

    private func liveStatus(chatId: String, now: Int64) -> SessionStatus? {
        if let demo = model.demo {
            return effectiveStatus(demo.sessions[chatId], now: now)
        }
        return effectiveStatus(model.workspace?.sessions[chatId], now: now)
    }

    /// Reserved 24pt status strip (shell.rs render_status_strip) — Working
    /// shows the sunrise spinner + rotating flavour word + elapsed; Errored
    /// shows "Run failed"; the strip always reserves its height so the
    /// composer never shifts.
    private func statusStrip(chatId: String, status: SessionStatus?, now: Int64) -> some View {
        HStack(spacing: 6) {
            switch status {
            case .working:
                WorkingSpinner()
                let startedAt = sessionStartedAt(chatId: chatId, now: now)
                let elapsedMs = now.subtractingReportingOverflow(startedAt)
                let elapsed = max(0, elapsedMs.overflow ? 0 : elapsedMs.partialValue / 1000)
                Text("\(Motion.flavourWord(seed: Motion.flavourSeed(chatId), elapsedSecs: elapsed))\u{2026}")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textMuted)
                Text(Motion.formatElapsed(elapsed))
                    .font(Theme.sans(11))
                    .foregroundStyle(Theme.textFaint)
                    .monospacedDigit()
            case .errored:
                Text("Run failed")
                    .font(Theme.sans(11))
                    .foregroundStyle(Theme.danger)
            default:
                EmptyView()
            }
        }
        .frame(height: 24)
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.leading, 26)  // aligns with the composer's text start
    }

    private func sessionStartedAt(chatId: String, now: Int64) -> Int64 {
        let row = model.demo?.sessions[chatId] ?? model.workspace?.sessions[chatId]
        return row?.startedAt ?? row?.updatedAt ?? now
    }
}
