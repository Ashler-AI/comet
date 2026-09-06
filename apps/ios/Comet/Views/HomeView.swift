// Home — the mobile shell. The desktop sidebar's Spaces and Sessions sections
// become the phone's home screen. Tabs-as-sessions don't fit a phone; a space
// opens into its own session list instead, and close=archive becomes
// swipe-to-archive. Imported memberships (sessions shared from another user's
// workspace, joined via a `comet://invite/…` link) close out the same Sessions
// list as globe rows; swipe-to-remove drops only workspace membership.

import SwiftUI

enum Route: Hashable {
    case space(String)
    case chat(String)
    case newSession(spaceId: String)
}

struct HomeView: View {
    @Environment(AppModel.self) private var model
    @State private var path: [Route] = []
    @State private var showNewSpace = false

    var body: some View {
        NavigationStack(path: $path) {
            List {
                spacesSection
                sessionsSection
            }
            .listStyle(.plain)
            .environment(\.defaultMinListRowHeight, 10)
            .contentMargins(.top, 2, for: .scrollContent)
            .scrollContentBackground(.hidden)
            .scrollEdgeEffectStyle(.soft, for: .top)
            .background(Theme.surface.ignoresSafeArea())
            .navigationTitle("Crew")  // feeds the back menu; not displayed
            .navigationBarTitleDisplayMode(.inline)
            .toolbar(removing: .title)
            .navigationDestination(for: Route.self) { route in
                switch route {
                case .space(let id): SpaceView(spaceId: id, path: $path)
                case .chat(let id): SessionView(chatId: id)
                case .newSession(let spaceId): NewSessionView(spaceId: spaceId, path: $path)
                }
            }
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    // In the bar, not the list: as a list row it appeared and
                    // vanished with the connection and shoved the content down.
                    if !model.connected {
                        ProgressView()
                            .controlSize(.mini)
                            .tint(Theme.textMuted)
                            .accessibilityLabel("Connecting")
                    }
                }
                // Bare spinner — no glass capsule behind it.
                .sharedBackgroundVisibility(.hidden)
                ToolbarItem(placement: .topBarTrailing) {
                    Button {
                        showNewSpace = true
                    } label: {
                        Image(systemName: "plus")
                    }
                    .accessibilityLabel("New space")
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Menu {
                        if model.demo != nil {
                            Text("Demo mode")
                        }
                        Button("Sign out", role: .destructive) { model.signOut() }
                    } label: {
                        Image(systemName: "person.circle")
                    }
                }
            }
            .sheet(isPresented: $showNewSpace) {
                NewSpaceSheet { spaceId in
                    path.append(.space(spaceId))
                }
            }
            .task(id: (model.overviewChats.map(\.id) + model.sharedSessionRefs.map(\.chatId)).joined()) {
                model.preloadSessions()
            }
            .onChange(of: model.launchRoute) { _, route in
                // Live one-click invite while Home is already up (cold-start
                // routes land via onAppear below).
                guard let route else { return }
                model.launchRoute = nil
                path.append(route)
            }
            .onAppear {
                if let route = model.launchRoute {
                    model.launchRoute = nil
                    // Push the whole stack atomically — appending from a child's
                    // onAppear mid-transition gets dropped by NavigationStack.
                    if case .space(let id) = route, model.launchSheet == "newsession" {
                        model.launchSheet = nil
                        path = [route, .newSession(spaceId: id)]
                    } else {
                        path = [route]
                    }
                }
                if model.launchSheet == "newspace" {
                    model.launchSheet = nil
                    showNewSpace = true
                }
            }
        }
    }

    // MARK: Spaces

    private var spacesSection: some View {
        let occupiedSpaceIds = Set(model.overviewChats.compactMap(\.spaceId))
        let spaces = model.spaces.filter { occupiedSpaceIds.contains($0.id) }
        return Section {
            if spaces.isEmpty {
                Text("Spaces with sessions will appear here — tap + to choose a folder")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textFaint)
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
            }
            ForEach(spaces) { space in
                Button {
                    path.append(.space(space.id))
                } label: {
                    SpaceRow(space: space)
                }
                .buttonStyle(PressWashButtonStyle())
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 2, leading: 12, bottom: 2, trailing: 12))
            }
        } header: {
            sectionHeader("Spaces")
        }
    }

    // MARK: Sessions

    private var sessionsSection: some View {
        Section {
            let chats = model.overviewChats
            let refs = model.sharedSessionRefs
            if chats.isEmpty && refs.isEmpty {
                Text("No sessions yet")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textFaint)
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
            }
            ForEach(chats) { chat in
                Button {
                    path.append(.chat(chat.id))
                } label: {
                    ChatRow(chat: chat, showLocation: true)
                }
                .buttonStyle(PressWashButtonStyle())
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 1, leading: 12, bottom: 1, trailing: 12))
                .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                    Button {
                        model.archive(chatId: chat.id)
                    } label: {
                        Label("Archive", systemImage: "archivebox")
                    }
                    .tint(Theme.surfaceRaised)
                }
            }
            .motionAnimation(Motion.resort, value: chats.map(\.id))
            // Imported memberships without a workspace chat row — the same
            // list, globe rows; removal drops only workspace membership.
            ForEach(refs) { sessionRef in
                Button {
                    path.append(.chat(sessionRef.chatId))
                } label: {
                    SharedSessionRow(sessionRef: sessionRef)
                }
                .buttonStyle(PressWashButtonStyle())
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 1, leading: 12, bottom: 1, trailing: 12))
                .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                    Button(role: .destructive) {
                        model.removeSessionRef(chatId: sessionRef.chatId)
                    } label: {
                        Label("Remove", systemImage: "minus.circle")
                    }
                }
            }
        } header: {
            sectionHeader("Sessions")
        }
    }
    private func sectionHeader(_ title: String) -> some View {
        Text(title)
            .font(Theme.sans(11, weight: .medium))
            .foregroundStyle(Theme.textMuted.opacity(0.6))
            .textCase(nil)
            .listRowInsets(EdgeInsets(top: 8, leading: 16, bottom: 3, trailing: 16))
    }
}

// MARK: - Rows

struct SpaceRow: View {
    @Environment(AppModel.self) private var model
    let space: Space

    var body: some View {
        HStack(spacing: 8) {
            // Leading 6pt aggregate dot — position stable, most-urgent member.
            TimelineView(.periodic(from: .now, by: 1)) { _ in
                let agg = model.spaceIndicator(space.id)
                Circle()
                    .fill((agg == .working || agg == .awaitingInput) ? (agg?.dotColor ?? whiteAlpha(0.14)) : whiteAlpha(0.14))
                    .frame(width: 6, height: 6)
            }
            Image(systemName: "folder")
                .font(.system(size: 13))
                .foregroundStyle(Theme.textMuted)
            Text(space.displayName)
                .font(Theme.sans(13, weight: .medium))
                .foregroundStyle(Theme.text)
                .lineLimit(1)
            Spacer(minLength: 8)
            deviceTag
            Image(systemName: "chevron.right")
                .font(.system(size: 10, weight: .medium))
                .foregroundStyle(Theme.textFaint.opacity(0.6))
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 8)
        .contentShape(RoundedRectangle(cornerRadius: 8))
    }

    private var deviceTag: some View {
        let online = model.deviceOnline(space.deviceId)
        let name = model.deviceName(space.deviceId)
        return Text(online ? "@ \(name)" : "@ \(name) · offline")
            .font(Theme.sans(12))
            .foregroundStyle(online ? Theme.textMuted.opacity(0.6) : Theme.warning.opacity(0.8))
            .lineLimit(1)
    }
}

/// Mobile port of the desktop rich session row (`shell.rs render_chat_row`):
/// context and top-right status share the first line, followed by title and
/// harness/branch metadata. Working replaces recency with a thin spinner;
/// completed, awaiting-input, and errored rows replace it with one blue
/// attention dot. Idle rows keep the recency label.
///
/// The phone adds the owning device to the context because this list
/// interleaves sessions hosted by every device.
struct ChatRow: View {
    @Environment(AppModel.self) private var model
    let chat: Chat
    var showLocation: Bool


    private var subline: Color { Theme.textMuted.opacity(0.5) }

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            // Unread, live status, and recency are independent signals.
            HStack(spacing: 8) {
                if showLocation {
                    Text(location)
                        .font(Theme.sans(11))
                        .foregroundStyle(subline)
                        .lineLimit(1)
                        .truncationMode(.tail)
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    Spacer(minLength: 4)
                }
                TimelineView(.periodic(from: .now, by: 1)) { timeline in
                    let now = Int64(timeline.date.timeIntervalSince1970 * 1000)
                    let activity = model.activity(chatId: chat.id, now: now)
                    HStack(spacing: 6) {
                        if chat.unseen {
                            Circle()
                                .fill(Theme.attention)
                                .frame(width: 7, height: 7)
                                .accessibilityLabel("Unread")
                        }
                        SessionStatusBadge(status: activity.status,
                                           sending: model.hasPendingSend(chatId: chat.id))
                        let updatedAt = max(chat.lastMessageAt ?? chat.createdAt, activity.row?.updatedAt ?? 0)
                        Text(relativeTime(updatedAt))
                            .font(Theme.sans(11))
                            .foregroundStyle(Theme.textMuted)
                            .fixedSize()
                            .accessibilityLabel("Last updated")
                            .accessibilityValue(Text(Date(timeIntervalSince1970: Double(updatedAt) / 1000), style: .relative))
                    }
                }
            }

            // Line 2: the session title.
            Text(chat.displayTitle)
                .font(Theme.sans(13))
                .foregroundStyle(Theme.text)
                .lineLimit(1)
                .frame(maxWidth: .infinity, alignment: .leading)

            // Line 3: harness brand mark, then the branch when the engine
            // stamped one.
            HStack(spacing: 4) {
                if let harness = chat.config?.harness {
                    HarnessBadge(harness: harness, size: 11, neutral: subline)
                }
                if let branch = chat.branch?.trimmingCharacters(in: .whitespaces), !branch.isEmpty {
                    LineIconView(.gitBranch, size: 11, color: subline)
                    Text(branch)
                        .font(Theme.sans(11))
                        .foregroundStyle(subline)
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
                Spacer(minLength: 0)
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .contentShape(RoundedRectangle(cornerRadius: 8))
    }


    /// "space · device", with offline marker. The space name (not the cwd
    /// basename) is what the desktop row shows — they differ once a space has
    /// been renamed, or when the session runs in a worktree off to the side.
    private var location: String {
        let space = model.space(for: chat)?.displayName
            ?? chat.cwd.map { ($0 as NSString).lastPathComponent }
            ?? "?"
        let name = model.deviceName(chat.deviceId)
        return model.deviceOnline(chat.deviceId)
            ? "\(space) · \(name)"
            : "\(space) · \(name) (offline)"
    }
}
struct SharedSessionRow: View {
    @Environment(AppModel.self) private var model
    let sessionRef: SessionRef

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { timeline in
            let now = Int64(timeline.date.timeIntervalSince1970 * 1000)
            let activity = model.activity(chatId: sessionRef.chatId, now: now)
            let updatedAt = max(sessionRef.addedAt, activity.row?.updatedAt ?? 0)
            HStack(spacing: 10) {
                Image(systemName: "globe")
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(Theme.textMuted.opacity(0.7))
                    .frame(width: 16)
                VStack(alignment: .leading, spacing: 2) {
                    Text(model.sessionTitle(for: sessionRef))
                        .font(Theme.sans(13))
                        .foregroundStyle(Theme.text)
                        .lineLimit(1)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    Text(relativeTime(updatedAt))
                        .font(Theme.sans(10.5))
                        .foregroundStyle(Theme.textMuted)
                        .accessibilityLabel("Last updated")
                        .accessibilityValue(Text(Date(timeIntervalSince1970: Double(updatedAt) / 1000), style: .relative))
                }
                SessionStatusBadge(status: activity.status,
                                   sending: model.hasPendingSend(chatId: sessionRef.chatId))
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 7)
        .contentShape(RoundedRectangle(cornerRadius: 8))
    }
}

private struct SessionStatusBadge: View {
    let status: SessionStatus?
    var sending = false

    var body: some View {
        Group {
            if sending || status == .working {
                HStack(spacing: 4) {
                    ArcSpinner()
                    Text(sending ? "Sending" : "Running")
                }
            } else if status == .awaitingInput {
                Text("Awaiting input").foregroundStyle(Theme.attention)
            } else if status == .errored {
                Text("Failed").foregroundStyle(Theme.danger)
            }
        }
        .font(Theme.sans(11))
        .foregroundStyle(Theme.textMuted)
        .fixedSize()
        .accessibilityElement(children: .combine)
    }
}


func relativeTime(_ ms: Int64) -> String {
    let delta = max(0, nowMs() - ms) / 1000
    if delta < 60 { return "now" }
    if delta < 3600 { return "\(delta / 60)m" }
    if delta < 86_400 { return "\(delta / 3600)h" }
    return "\(delta / 86_400)d"
}
