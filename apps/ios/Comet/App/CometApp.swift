// Comet for iOS — a viewport onto the comet-native mesh. The phone is a peer
// device: it joins the workspace and session doc rooms and drives remote
// engines through the durable command queue.

import SwiftUI

@main
struct CometApp: App {
    @State private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(model)
                .preferredColorScheme(.dark)
                // Monochrome controls: glass buttons, toolbar icons, and
                // toggles render white like the desktop — accent stays paint
                // for status/markdown, never chrome.
                .tint(Theme.text)
                .background(Theme.bg)
                // One-click session invitations (`comet://invite/…`) — the
                // same deep link the desktop copies from the invite dialog.
                .onOpenURL { url in
                    model.openInvitation(url: url)
                }
                .onChange(of: scenePhase) { _, phase in
                    if phase == .background {
                        model.flushDocs()
                    }
                }
        }
    }
}

struct RootView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Group {
            switch model.phase {
            case .signedOut:
                SignInView()
            case .ready:
                HomeView()
            }
        }
        .task { model.restore() }
    }
}
