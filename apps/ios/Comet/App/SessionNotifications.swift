import Foundation
import Observation
import UIKit
import UserNotifications
import os

private let notificationLog = Logger(subsystem: "dev.cometnative.Comet", category: "notifications")

/// Factual transitions only: replay, heartbeats and initial hydration are silent.
struct AttentionTracker {
    private var previous: [String: SessionRow] = [:]

    mutating func update(_ sessions: [String: SessionRow], chats: [Chat], now: Int64) -> [(String, String)] {
        var alerts: [(String, String)] = []
        for (chatId, row) in sessions {
            guard let old = previous[chatId] else {
                previous[chatId] = row
                continue
            }
            guard row.updatedAt > old.updatedAt else { continue }
            previous[chatId] = row
            guard now - row.updatedAt <= sessionStaleMs, row.status != old.status,
                  chats.contains(where: { $0.id == chatId && !$0.archived }) else { continue }
            let body: String
            switch row.status {
            case .awaitingInput: body = "Needs your input."
            case .errored: body = "Encountered an error."
            case .idle where old.status == .working: body = "Finished working."
            default: continue
            }
            alerts.append((chatId, body))
        }
        return alerts
    }
}

@MainActor
@Observable
final class SessionNotifications: NSObject, UNUserNotificationCenterDelegate {
    static let shared = SessionNotifications()
    private(set) var enabled = UserDefaults.standard.bool(forKey: "sessionNotificationsEnabled")
    private(set) var busy = false
    private(set) var status = "Notifications are off."
    var error: String?
    var visibleChatId: String?
    var openSession: ((String) -> Void)?
    @ObservationIgnored private var config: AppConfig?
    @ObservationIgnored private var token: String?
    @ObservationIgnored private var registeredToken: String?
    @ObservationIgnored private var tracker = AttentionTracker()
    @ObservationIgnored private var pendingTarget: [AnyHashable: Any]?
    @ObservationIgnored private var operation: Task<Void, Never>?
    @ObservationIgnored private var generation: UInt64 = 0
    @ObservationIgnored private var operationId: UInt64 = 0
    private let center = UNUserNotificationCenter.current()
    @ObservationIgnored private var session = URLSession.shared
    #if DEBUG
    @ObservationIgnored private var regressionFinished = false
    #endif

    private var installationId: String {
        if let id = UserDefaults.standard.string(forKey: "notificationInstallationId") { return id }
        let id = UUID().uuidString.lowercased()
        UserDefaults.standard.set(id, forKey: "notificationInstallationId")
        return id
    }

    override private init() {
        super.init()
        center.delegate = self
    }

    func configure(_ config: AppConfig, openSession: @escaping (String) -> Void) {
        self.config = config
        self.openSession = openSession
        tracker = AttentionTracker()
        registeredToken = nil
        if let pendingTarget {
            self.pendingTarget = nil
            open(pendingTarget)
        }
        refresh()
    }

    /// APNs callbacks join the same queue even while permission or network work
    /// is suspended. Signing out invalidates every queued state mutation.
    @discardableResult
    private func enqueue(_ work: @escaping @MainActor (UInt64) async -> Void) -> Task<Void, Never> {
        let previous = operation
        let epoch = generation
        operationId &+= 1
        let id = operationId
        busy = true
        let task = Task { @MainActor in
            await previous?.value
            guard generation == epoch, !Task.isCancelled else { return }
            await work(epoch)
            if generation == epoch && operationId == id {
                busy = false
                operation = nil
            }
        }
        operation = task
        return task
    }

    func refresh() {
        guard enabled, config != nil, !busy else { return }
        enqueue { [self] epoch in
            let settings = await center.notificationSettings()
            guard generation == epoch else { return }
            guard settings.authorizationStatus == .authorized || settings.authorizationStatus == .provisional else {
                status = "Allow notifications in iOS Settings to receive session alerts."
                return
            }
            UIApplication.shared.registerForRemoteNotifications()
            await registerToken(generation: epoch, renew: true)
        }
    }

    func setEnabled(_ value: Bool) async {
        guard !busy, config != nil else { return }
        await enqueue { [self] epoch in
            error = nil
            if value {
                do {
                    let allowed = try await center.requestAuthorization(options: [.alert, .sound])
                    guard generation == epoch else { return }
                    guard allowed else {
                        status = "Notifications are blocked. Allow them in iOS Settings."
                        return
                    }
                    enabled = true
                    UserDefaults.standard.set(true, forKey: "sessionNotificationsEnabled")
                    status = "Connecting background notifications…"
                    UIApplication.shared.registerForRemoteNotifications()
                    await registerToken(generation: epoch)
                } catch {
                    if generation == epoch {
                        self.error = "Could not enable notifications: \(error.localizedDescription)"
                    }
                }
            } else {
                guard await unregister(generation: epoch), generation == epoch else { return }
                enabled = false
                UserDefaults.standard.set(false, forKey: "sessionNotificationsEnabled")
                center.removeAllPendingNotificationRequests()
                center.removeAllDeliveredNotifications()
                status = "Notifications are off."
            }
        }.value
    }

    func didRegister(_ data: Data) {
        let next = data.map { String(format: "%02x", $0) }.joined()
        guard config != nil, token != next else { return }
        token = next
        enqueue { [self] epoch in await registerToken(generation: epoch) }
    }

    func didFailRegistration() {
        status = "Background notifications unavailable. Alerts work only while Crew is running."
    }

    private func registerToken(generation epoch: UInt64, renew: Bool = false) async {
        guard generation == epoch, enabled, let config, let token,
              renew || registeredToken != token else { return }
        UserDefaults.standard.set(true, forKey: "notificationRegistrationPending")
        do {
            try await request(config, method: "PUT", values: [
                "installationId": installationId, "token": token, "environment": Self.pushEnvironment
            ])
            guard generation == epoch else { return }
            registeredToken = token
            status = "Session alerts are enabled, including when Crew is closed."
            error = nil
        } catch {
            guard generation == epoch else { return }
            registeredToken = nil
            status = "Alerts work while Crew is running; background delivery is unavailable."
            self.error = "Could not register background notifications: \(error.localizedDescription)"
        }
    }

    /// Local logout never waits on network or a pending permission response.
    /// A best-effort DELETE runs after any in-flight PUT, using only the old
    /// in-memory credential; APNs also invalidates the unregistered device token.
    /// Callers disposing network resources can await the returned cleanup task.
    @discardableResult
    func signOut() -> Task<Void, Never>? {
        let oldConfig = config
        let oldInstallationId = installationId
        let pending = operation
        let needsDelete = UserDefaults.standard.bool(forKey: "notificationRegistrationPending")
        generation &+= 1
        operation = nil
        busy = false
        config = nil
        token = nil
        registeredToken = nil
        openSession = nil
        pendingTarget = nil
        visibleChatId = nil
        tracker = AttentionTracker()
        error = nil
        status = "Sign in to receive session notifications."
        UserDefaults.standard.set(false, forKey: "notificationRegistrationPending")
        UserDefaults.standard.removeObject(forKey: "notificationInstallationId")
        UIApplication.shared.unregisterForRemoteNotifications()
        center.removeAllPendingNotificationRequests()
        center.removeAllDeliveredNotifications()
        if let oldConfig, needsDelete {
            return Task { [self] in
                await pending?.value
                try? await request(oldConfig, method: "DELETE", values: ["installationId": oldInstallationId])
            }
        }
        return pending
    }

    private func unregister(generation epoch: UInt64) async -> Bool {
        guard let config else { return true }
        // A previous process may have registered even when this process has no token.
        guard UserDefaults.standard.bool(forKey: "notificationRegistrationPending") else { return true }
        do {
            try await request(config, method: "DELETE", values: ["installationId": installationId])
            guard generation == epoch else { return false }
            registeredToken = nil
            UserDefaults.standard.set(false, forKey: "notificationRegistrationPending")
            error = nil
            return true
        } catch {
            guard generation == epoch else { return false }
            self.error = "Could not disable background notifications. Reconnect and try again."
            return false
        }
    }

    private func request(_ config: AppConfig, method: String, values: [String: String]) async throws {
        guard let bearer = await config.currentToken() else { throw URLError(.userAuthenticationRequired) }
        #if DEBUG
        // A timed-out regression may still have a queued logout DELETE. Never
        // let it create a URLSession task after the probe has been disposed.
        guard !regressionFinished else { throw URLError(.cancelled) }
        #endif
        var request = URLRequest(url: config.edgeURL.appending(path: "notifications/device"))
        request.httpMethod = method
        request.timeoutInterval = 20
        request.setValue("Bearer \(bearer)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONSerialization.data(withJSONObject: values)
        let (_, response) = try await session.data(for: request)
        guard let response = response as? HTTPURLResponse else { throw URLError(.badServerResponse) }
        // A revoked or no-longer-authorized credential cannot pass the server's
        // per-delivery authorization check; it must not trap the user signed in.
        if method == "DELETE", response.statusCode == 401 || response.statusCode == 403 { return }
        guard (200..<300).contains(response.statusCode) else {
            throw URLError(.badServerResponse)
        }
    }

    /// Distribution re-signing determines APNs environment, not the Swift debug flag.
    private static var pushEnvironment: String {
        #if targetEnvironment(simulator)
        return "sandbox"
        #else
        if let url = Bundle.main.url(forResource: "embedded", withExtension: "mobileprovision"),
           let data = try? Data(contentsOf: url),
           let start = data.range(of: Data("<?xml".utf8)),
           let end = data.range(of: Data("</plist>".utf8), in: start.lowerBound..<data.endIndex),
           let plist = try? PropertyListSerialization.propertyList(from: data[start.lowerBound..<end.upperBound], format: nil),
           let root = plist as? [String: Any], let entitlements = root["Entitlements"] as? [String: Any],
           entitlements["aps-environment"] as? String == "development" {
            return "sandbox"
        }
        return "production"
        #endif
    }

    static func notificationTitle(_ title: String?, chatId: String) -> String {
        let title = normalizedSessionTitle(title) ?? "Session \(chatId.prefix(8))"
        let end = title.index(title.startIndex, offsetBy: 120, limitedBy: title.endIndex)
        guard let end, end != title.endIndex else { return title }
        return String(title.prefix(119)) + "…"
    }

    func update(sessions: [String: SessionRow], chats: [Chat], resolveTitle: (String) -> String?) {
        let alerts = tracker.update(sessions, chats: chats, now: nowMs())
        guard enabled, registeredToken == nil, let config else { return }
        for (chatId, body) in alerts {
            guard !(UIApplication.shared.applicationState == .active && visibleChatId == chatId) else { continue }
            let content = UNMutableNotificationContent()
            content.title = Self.notificationTitle(resolveTitle(chatId), chatId: chatId)
            content.body = body
            content.sound = .default
            content.threadIdentifier = chatId
            content.userInfo = ["chatId": chatId, "userId": config.userId, "projectScope": config.projectScope]
            center.add(UNNotificationRequest(identifier: "session-\(chatId)", content: content, trigger: nil)) { error in
                guard let error else { return }
                let failure = error as NSError
                notificationLog.error("Local session notification rejected: domain=\(failure.domain, privacy: .public) code=\(failure.code)")
            }
        }
    }

    private func matchesIdentity(_ info: [AnyHashable: Any]) -> Bool {
        guard let config else { return false }
        return info["userId"] as? String == config.userId && info["projectScope"] as? String == config.projectScope
    }

    private func open(_ info: [AnyHashable: Any]) {
        guard config != nil else { pendingTarget = info; return }
        guard matchesIdentity(info), let chatId = info["chatId"] as? String else { return }
        openSession?(chatId)
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void) {
        let info = notification.request.content.userInfo
        Task { @MainActor in
            let visible = UIApplication.shared.applicationState == .active && self.visibleChatId == info["chatId"] as? String
            completionHandler(self.enabled && self.matchesIdentity(info) && !visible ? [.banner, .sound, .list] : [])
        }
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void) {
        let info = response.notification.request.content.userInfo
        Task { @MainActor in
            self.open(info)
            completionHandler()
        }
    }

    #if DEBUG
    /// Exercises queued token arrival, offline disable/logout and late PUT
    /// responses without requesting OS permission or sending any real traffic.
    static func runLifecycleRegression() async {
        let oldDelegate = UNUserNotificationCenter.current().delegate
        let keys = ["notificationInstallationId", "notificationRegistrationPending", "sessionNotificationsEnabled"]
        let saved = keys.map { UserDefaults.standard.object(forKey: $0) }
        let probe = SessionNotifications()
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [NotificationRegressionProtocol.self]
        probe.session = URLSession(configuration: configuration)
        let queued = NotificationRegressionGate()
        let put = NotificationRegressionGate()
        let delete = NotificationRegressionGate()
        var tasks: [Task<Void, Never>] = []
        defer {
            // Close admission before cancelling requests: cancellation can wake
            // the PUT queue and start its separate logout cleanup task.
            probe.regressionFinished = true
            probe.generation &+= 1
            probe.config = nil
            probe.operation?.cancel()
            for task in tasks { task.cancel() }
            queued.released = true
            put.released = true
            delete.released = true
            probe.session.invalidateAndCancel()
            NotificationRegressionProtocol.respond = nil
            UNUserNotificationCenter.current().delegate = oldDelegate
            for (key, value) in zip(keys, saved) { UserDefaults.standard.set(value, forKey: key) }
        }
        probe.enabled = true
        probe.config = AppConfig(edgeURL: URL(string: "http://localhost")!, mode: .dev,
            userId: "notification-test", projectScope: "notification-test", deviceId: "phone",
            deviceName: "Crew regression", devBearer: "notification-test@notification-test")
        NotificationRegressionProtocol.respond = { _ in 200 }
        tasks.append(probe.enqueue { _ in _ = await queued.wait(stage: "queued token release") })
        guard await notificationRegressionWait(stage: "queued token start", { queued.entered }) else { return }
        let first = Data(repeating: 1, count: 32)
        probe.didRegister(first)
        queued.released = true
        guard await notificationRegressionWait(stage: "queued token completion", { !probe.busy }) else { return }
        guard probe.registeredToken == String(repeating: "01", count: 32) else {
            E2ERunner.log("FAIL Crew APNs callback was lost while busy")
            return
        }
        var opened: String?
        probe.openSession = { opened = $0 }
        probe.open(["chatId": "legacy-SESSION", "userId": "other", "projectScope": "notification-test"])
        guard opened == nil else { E2ERunner.log("FAIL Crew notification crossed identity"); return }
        probe.open(["chatId": "legacy-SESSION", "userId": "notification-test", "projectScope": "notification-test"])
        guard opened == "legacy-SESSION" else { E2ERunner.log("FAIL Crew notification lost routing ID"); return }

        NotificationRegressionProtocol.respond = { _ in throw URLError(.notConnectedToInternet) }
        var disabled = false
        tasks.append(Task { await probe.setEnabled(false); disabled = true })
        guard await notificationRegressionWait(stage: "offline disable completion", { disabled }) else { return }
        guard probe.enabled, probe.error != nil else { E2ERunner.log("FAIL Crew offline disable pretended success"); return }
        let args = ProcessInfo.processInfo.arguments
        NotificationRegressionProtocol.respond = { request in
            if request.httpMethod == "PUT" {
                guard await put.wait(stage: "delayed PUT release") else { throw URLError(.cancelled) }
                return 200
            }
            // Debug-only fault injection: leave the URLSession request pending
            // without invoking its client, so teardown must cancel it safely.
            if args.contains("-notification-e2e-missing-delete") { return nil }
            guard await delete.wait(stage: "delayed DELETE release") else { throw URLError(.cancelled) }
            throw URLError(.notConnectedToInternet)
        }
        probe.didRegister(Data(repeating: 2, count: 32))
        guard await notificationRegressionWait(stage: "delayed PUT responder", { put.entered }) else { return }
        if args.contains("-notification-e2e-missing-token") {
            // The PUT already holds its credential. Logout gets none and never
            // reaches the protocol responder; the arrival deadline must fail.
            probe.config = AppConfig(edgeURL: URL(string: "http://localhost")!, mode: .dev,
                userId: "notification-test", projectScope: "notification-test", deviceId: "phone",
                deviceName: "Crew regression")
        }
        let pending = probe.operation
        let cleanup = probe.signOut()
        let signedOutImmediately = probe.config == nil && !probe.busy && probe.registeredToken == nil
        if let pending { tasks.append(pending) }
        if let cleanup { tasks.append(cleanup) }
        var drained = false
        tasks.append(Task {
            await pending?.value
            await cleanup?.value
            drained = true
        })
        put.released = true
        guard await notificationRegressionWait(stage: "logout DELETE responder", { delete.entered }) else { return }
        delete.released = true
        guard await notificationRegressionWait(stage: "logout cleanup drain", { drained }) else { return }
        guard !queued.failed, !put.failed, !delete.failed else { return }
        guard signedOutImmediately else {
            E2ERunner.log("FAIL Crew logout waited for push deletion")
            return
        }
        guard probe.config == nil, probe.registeredToken == nil else {
            E2ERunner.log("FAIL Crew late registration revived signed-out state")
            return
        }
        E2ERunner.log("OK Crew APNs lifecycle: queued token, scoped route, offline disable, immediate logout, late-response isolation, drained logout DELETE")
    }
    #endif
}

final class NotificationAppDelegate: NSObject, UIApplicationDelegate {
    func application(_ application: UIApplication, didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil) -> Bool {
        _ = SessionNotifications.shared
        return true
    }

    func application(_ application: UIApplication, didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data) {
        SessionNotifications.shared.didRegister(deviceToken)
    }

    func application(_ application: UIApplication, didFailToRegisterForRemoteNotificationsWithError error: Error) {
        SessionNotifications.shared.didFailRegistration()
    }
}

#if DEBUG
@MainActor
private func notificationRegressionWait(stage: String, _ ready: () -> Bool) async -> Bool {
    let deadline = ContinuousClock.now.advanced(by: .seconds(5))
    while ContinuousClock.now < deadline {
        if Task.isCancelled { return false }
        if ready() { return true }
        do { try await Task.sleep(nanoseconds: 10_000_000) }
        catch { return false }
    }
    E2ERunner.log("FAIL Crew APNs lifecycle deadline: \(stage)")
    return false
}

@MainActor
private final class NotificationRegressionGate {
    var entered = false
    var released = false
    var failed = false

    func wait(stage: String) async -> Bool {
        entered = true
        let completed = await notificationRegressionWait(stage: stage, { released })
        failed = !completed
        return completed
    }
}

private final class NotificationRegressionProtocol: URLProtocol {
    @MainActor static var respond: ((URLRequest) async throws -> Int?)?
    private let lock = NSRecursiveLock()
    private var stopped = false
    private var loading: Task<Void, Never>?

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        lock.withLock {
            guard !stopped else { return }
            loading = Task { @MainActor in
                do {
                    try Task.checkCancellation()
                    guard let respond = Self.respond else { throw URLError(.cancelled) }
                    guard let status = try await respond(request) else { return }
                    lock.withLock {
                        guard !stopped, !Task.isCancelled else { return }
                        let response = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: nil, headerFields: nil)!
                        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
                        // A client may synchronously stop loading from didReceive.
                        guard !stopped else { return }
                        stopped = true
                        client?.urlProtocolDidFinishLoading(self)
                        loading = nil
                    }
                } catch {
                    lock.withLock {
                        guard !stopped, !Task.isCancelled else { return }
                        stopped = true
                        client?.urlProtocol(self, didFailWithError: error)
                        loading = nil
                    }
                }
            }
        }
    }

    override func stopLoading() {
        lock.withLock {
            stopped = true
            loading?.cancel()
            loading = nil
        }
    }
}
#endif
