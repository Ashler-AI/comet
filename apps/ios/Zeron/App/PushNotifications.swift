import UIKit
import UserNotifications

/// Session notifications, like the desktop's: a run finished, a session is
/// waiting on you, a run failed. The edge decides and sends them (APNs); this
/// asks for permission, hands the edge this phone's token and choices, stays
/// quiet while the app is open (the desktop's "only in the background"), and
/// opens the session a notification is about.
@MainActor
final class PushNotifications: NSObject, UNUserNotificationCenterDelegate {
    static let shared = PushNotifications()

    enum Kind: String, CaseIterable {
        case done, input, failed

        var title: String {
            switch self {
            case .done: "Run finished"
            case .input: "Waiting on your input"
            case .failed: "Run failed"
            }
        }
    }

    private var currentClient: () -> CoreClient?
    private var accountGeneration: UInt64 = 0
    private(set) var pendingRegistration: Task<Void, Never>?
    private var token: String?
    private struct Target: Sendable {
        let chatId: String
        let userId: String
        let projectScope: String
        let deploymentId: String?
    }
    /// Retain identity with cold-start taps; never reinterpret a tap after account switch.
    private var pendingTarget: Target?
    /// Opens a chat once the shell is up (set by the scene).
    var openChat: ((String) -> Bool)?

    private let defaults: UserDefaults
    private static let enabledKey = "notifications.enabled"
    private static let askedKey = "notifications.asked"

    init(defaults: UserDefaults = .standard, currentClient: @escaping () -> CoreClient? = { nil }) {
        self.defaults = defaults
        self.currentClient = currentClient
        super.init()
    }

    // MARK: Choices

    /// The master switch (on unless turned off here; the system permission
    /// is separate).
    var enabled: Bool {
        get { defaults.object(forKey: Self.enabledKey) as? Bool ?? true }
        set {
            defaults.set(newValue, forKey: Self.enabledKey)
            sync()
        }
    }

    func isOn(_ kind: Kind) -> Bool { defaults.object(forKey: "notifications.\(kind.rawValue)") as? Bool ?? true }

    func set(_ kind: Kind, _ on: Bool) {
        defaults.set(on, forKey: "notifications.\(kind.rawValue)")
        sync()
    }

    private var prefs: PushPrefs { PushPrefs(done: isOn(.done), input: isOn(.input), failed: isOn(.failed)) }

    // MARK: Lifecycle

    /// At launch: route taps; once signed in, keep the edge's registration fresh.
    func install(app: AppModel) {
        currentClient = { [weak app] in app?.client }
        UNUserNotificationCenter.current().delegate = self
    }

    /// Refresh on sign-in, foreground and network recovery if already allowed.
    /// Tokens can change between launches; the edge keeps the latest.
    func signedIn() {
        guard let client = currentClient(), !client.isDemo() else { return }
        let generation = accountGeneration
        Task {
            let settings = await UNUserNotificationCenter.current().notificationSettings()
            guard generation == accountGeneration, currentClient() === client else { return }
            if Self.allowed(settings.authorizationStatus) {
                UIApplication.shared.registerForRemoteNotifications()
            }
        }
    }

    /// The first time a session is started from this phone: the moment the
    /// ask makes sense ("tell me when it's done").
    func askAfterFirstSession() {
        guard let client = currentClient(), !client.isDemo(), enabled, !defaults.bool(forKey: Self.askedKey) else { return }
        defaults.set(true, forKey: Self.askedKey)
        let generation = accountGeneration
        Task {
            guard generation == accountGeneration, currentClient() === client else { return }
            _ = await self.requestPermission()
        }
    }

    /// Ask the system (once; later it's the Settings app's call).
    @discardableResult
    func requestPermission() async -> Bool {
        guard let client = currentClient() else { return false }
        let generation = accountGeneration
        let center = UNUserNotificationCenter.current()
        let status = await center.notificationSettings().authorizationStatus
        guard generation == accountGeneration, currentClient() === client else { return false }
        let granted: Bool
        if status == .notDetermined {
            granted = (try? await center.requestAuthorization(options: [.alert, .sound, .badge])) ?? false
        } else {
            granted = Self.allowed(status)
        }
        guard generation == accountGeneration, currentClient() === client else { return false }
        if granted { UIApplication.shared.registerForRemoteNotifications() }
        return granted
    }

    func authorizationStatus() async -> UNAuthorizationStatus {
        await UNUserNotificationCenter.current().notificationSettings().authorizationStatus
    }

    private static func allowed(_ status: UNAuthorizationStatus) -> Bool {
        status == .authorized || status == .provisional || status == .ephemeral
    }

    /// Signing out: this phone stops getting the account's notifications.
    func signingOut(client: CoreClient?) {
        accountGeneration &+= 1
        token = nil
        pendingTarget = nil
        if let client {
            let previous = pendingRegistration
            pendingRegistration = Task {
                // An in-flight upsert must finish before this account's removal.
                await previous?.value
                do { try await client.unregisterPushTarget() }
                catch { NSLog("push target removal failed: \(error)") }
            }
        }
        UIApplication.shared.unregisterForRemoteNotifications()
    }

    // MARK: Token

    func didRegister(deviceToken: Data) {
        token = deviceToken.map { String(format: "%02x", $0) }.joined()
        sync()
    }

    func didFailToRegister(_ error: Error) {
        NSLog("push registration failed: \(error)")
    }

    /// Tell the edge what this phone wants now (or that it wants nothing).
    private func sync() {
        guard let client = currentClient(), !client.isDemo() else { return }
        let generation = accountGeneration
        let previous = pendingRegistration
        pendingRegistration = Task {
            await previous?.value
            guard generation == accountGeneration, currentClient() === client else { return }
            // Read choices after waiting: an older queued refresh cannot restore
            // stale preferences, and sign-out invalidates every queued upsert.
            do {
                if enabled, let token {
                    try await client.registerPushTarget(token: token, environment: Self.apnsEnvironment, prefs: prefs)
                } else if !enabled {
                    try await client.unregisterPushTarget()
                }
            } catch {
                NSLog("push target sync failed: \(error)")
            }
        }
    }

    /// Which APNs this build's tokens belong to: development-signed builds
    /// (Xcode) say so in their provisioning profile; App Store / TestFlight
    /// builds carry none and are production.
    static let apnsEnvironment: String = {
        #if targetEnvironment(simulator)
        return "sandbox"
        #else
        guard let url = Bundle.main.url(forResource: "embedded", withExtension: "mobileprovision"),
              let data = try? Data(contentsOf: url),
              let text = String(data: data, encoding: .isoLatin1)
        else { return "production" }
        return text.range(of: "<key>aps-environment</key>\\s*<string>development</string>", options: .regularExpression) != nil
            ? "sandbox" : "production"
        #endif
    }()

    // MARK: Delivery

    // Completion-handler forms, on the main queue: the async variants
    // complete off the main thread, which UIKit asserts on (crash on tap).

    /// In the app: no banner (the list and the session already show it).
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        completionHandler([])
    }

    /// Tapped: open that session.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        let info = response.notification.request.content.userInfo
        let chatId = info["chatId"] as? String
        let userId = info["userId"] as? String
        let projectScope = info["projectScope"] as? String
        let deploymentId = info["deploymentId"] as? String
        let malformedDeployment = info["deploymentId"] != nil && deploymentId == nil
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                if let chatId, !chatId.isEmpty, let userId, let projectScope, !malformedDeployment {
                    self.open(Target(chatId: chatId, userId: userId, projectScope: projectScope, deploymentId: deploymentId))
                }
            }
            completionHandler()
        }
    }

    nonisolated static func matchesScope(userId: String, projectScope: String, deploymentId: String?,
                                         currentUser: String, currentProject: String, currentDeployment: String?) -> Bool {
        !userId.isEmpty && !projectScope.isEmpty && userId == currentUser && projectScope == currentProject
            && (deploymentId == nil || deploymentId == currentDeployment)
    }

    private func open(_ target: Target) {
        guard let client = currentClient() else { pendingTarget = target; return }
        guard Self.matchesScope(userId: target.userId, projectScope: target.projectScope, deploymentId: target.deploymentId,
                                currentUser: client.userId(), currentProject: client.orgId(), currentDeployment: Endpoints.deploymentId)
        else { pendingTarget = nil; return }
        if openChat?(target.chatId) != true { pendingTarget = target }
    }

    /// The shell is up: open a chat tapped before it was.
    func shellReady() {
        guard let target = pendingTarget else { return }
        pendingTarget = nil
        open(target)
    }
}
