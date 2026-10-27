import XCTest
@testable import Zeron

final class CrewIsolationTests: XCTestCase {
    func testStagingIdentityAndProfileIsolation() throws {
        XCTAssertEqual(Bundle.main.bundleIdentifier, "ai.ashler.crew.staging")
        XCTAssertEqual(Endpoints.callbackScheme, "comet-staging")
        XCTAssertEqual(try Endpoints.setting("CrewProjectScope"), "ashler-staging")
        XCTAssertEqual(try Endpoints.setting("CrewDeploymentId"), "ashler-staging")
        XCTAssertTrue(AppModel.profilesDir.path.hasSuffix("profiles/scaffold"))
    }

    func testNotificationScopeNeverCrossesAccountsOrDeployment() {
        func accepts(_ user: String, _ project: String, _ deployment: String?) -> Bool {
            PushNotifications.matchesScope(userId: user, projectScope: project, deploymentId: deployment,
                                           currentUser: "owner", currentProject: "ashler-staging", currentDeployment: "ashler-staging")
        }
        XCTAssertTrue(accepts("owner", "ashler-staging", "ashler-staging"))
        XCTAssertTrue(accepts("owner", "ashler-staging", nil), "Existing legacy payloads bind user and project")
        XCTAssertFalse(accepts("foreign", "ashler-staging", nil))
        XCTAssertFalse(accepts("owner", "ashler-production", nil))
        XCTAssertFalse(accepts("owner", "ashler-staging", "foreign-deployment"))
        XCTAssertFalse(accepts("", "", nil))
    }

    @MainActor
    func testCreatingSandboxNeverReusesSelectedSessionIdentity() throws {
        let selected = UUID().uuidString.lowercased()
        let created = try CrewControlsViewController.sessionID(for: "create", selected: selected)
        XCTAssertNotEqual(created, selected)
        XCTAssertNotNil(UUID(uuidString: created))
        XCTAssertNotEqual(created, try CrewControlsViewController.sessionID(for: "create", selected: selected))
        XCTAssertEqual(try CrewControlsViewController.sessionID(for: "attach", selected: selected), selected)
        XCTAssertEqual(try CrewControlsViewController.sessionID(for: "start", selected: selected), selected)
        XCTAssertThrowsError(try CrewControlsViewController.sessionID(for: "start", selected: nil))
        XCTAssertTrue(CrewControlsViewController.canStartNewEpoch(lifecycle: "stopped", epoch: 7))
        XCTAssertTrue(CrewControlsViewController.canStartNewEpoch(lifecycle: "failed", epoch: 7))
        XCTAssertFalse(CrewControlsViewController.canStartNewEpoch(lifecycle: "stopped", epoch: nil))
        XCTAssertEqual(CrewControlsViewController.lifecycleEpoch(from: NSNumber(value: UInt64.max)), UInt64.max)
        for value in [nil, true, -1, 1.5, "7"] as [Any?] {
            XCTAssertNil(CrewControlsViewController.lifecycleEpoch(from: value))
        }
        for lifecycle in [nil, "running", "paused", "starting", "unknown"] as [String?] {
            XCTAssertFalse(CrewControlsViewController.canStartNewEpoch(lifecycle: lifecycle, epoch: 7))
        }
        XCTAssertThrowsError(try CrewControlsViewController.sessionID(for: "attach", selected: nil))
        XCTAssertThrowsError(try CrewControlsViewController.sessionID(for: "inspect", selected: "invalid"))
    }

    func testCoreErrorPreservesItsMessageThroughFoundation() {
        let message = "Access denied · \(UUID().uuidString)\nUse the authenticated project."
        let error: Error = CoreError.Auth(message: message)
        XCTAssertEqual(error.localizedDescription, message)
    }

    #if DEBUG && targetEnvironment(simulator)
    func testSimulatorSessionImportConsumesOnlyBoundedPrivateFiles() throws {
        let files = FileManager.default
        let dir = files.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try files.createDirectory(at: dir, withIntermediateDirectories: false)
        defer { try? files.removeItem(at: dir) }
        let input = dir.appendingPathComponent("import.json")
        let contents = Data("{\"accessToken\":\"sc_rc_test_fixture\"}".utf8)
        XCTAssertTrue(files.createFile(atPath: input.path, contents: contents, attributes: [.posixPermissions: 0o600]))
        XCTAssertEqual(try AppModel.consumeSimulatorSession(at: input), String(decoding: contents, as: UTF8.self))
        XCTAssertFalse(files.fileExists(atPath: input.path), "A consumed bearer must not remain in a transfer file")
        for (data, mode) in [(contents, 0o644), (Data(repeating: 65, count: 65_537), 0o600), (Data([0xff]), 0o600)] {
            XCTAssertTrue(files.createFile(atPath: input.path, contents: data, attributes: [.posixPermissions: mode]))
            XCTAssertThrowsError(try AppModel.consumeSimulatorSession(at: input))
            XCTAssertFalse(files.fileExists(atPath: input.path), "Rejected transfer files must also be consumed")
        }
        let source = dir.appendingPathComponent("source.json")
        XCTAssertTrue(files.createFile(atPath: source.path, contents: contents, attributes: [.posixPermissions: 0o600]))
        try files.createSymbolicLink(at: input, withDestinationURL: source)
        XCTAssertThrowsError(try AppModel.consumeSimulatorSession(at: input))
        XCTAssertEqual(try Data(contentsOf: source), contents, "Never consume a symlink target")
        try files.removeItem(at: input)
        try files.linkItem(at: source, to: input)
        XCTAssertThrowsError(try AppModel.consumeSimulatorSession(at: input))
        XCTAssertEqual(try Data(contentsOf: source), contents, "Never consume a hard-linked source")
    }
    #endif

    func testAccountSwitchRetainsDraftWithoutDisclosingIt() {
        let oldProfile = Drafts.profile
        let first = "test-crew-" + UUID().uuidString
        let second = "test-crew-" + UUID().uuidString
        defer {
            Drafts.profile = oldProfile
            UserDefaults.standard.removeObject(forKey: "drafts.\(first)")
            UserDefaults.standard.removeObject(forKey: "drafts.\(second)")
        }
        Drafts.profile = first
        Drafts.save("same-session", "unsent private work")
        Drafts.profile = second
        XCTAssertEqual(Drafts.load("same-session"), "")
        Drafts.profile = first
        XCTAssertEqual(Drafts.load("same-session"), "unsent private work")
    }

    @MainActor
    func testNotificationTeardownRemovesInFlightTargetBeforeAccountReplacement() async {
        for replaceAccount in [false, true] {
            let suite = "notification-order-\(UUID().uuidString)"
            let defaults = UserDefaults(suiteName: suite)!
            defer { defaults.removePersistentDomain(forName: suite) }
            let targets = NotificationTargets()
            let first = NotificationClient(owner: "first", targets: targets)
            let second = NotificationClient(owner: "second", targets: targets)
            var current: CoreClient? = first
            let notifications = PushNotifications(defaults: defaults, currentClient: { current })
            notifications.didRegister(deviceToken: Data([1]))
            await fulfillment(of: [targets.writeStarted], timeout: 2)
            notifications.set(.done, false) // Queued while the old account's POST is in flight.
            // Both signOutLocally and direct signIn(account:) call this boundary
            // before retiring/replacing their old CoreClient.
            notifications.signingOut(client: first)
            current = replaceAccount ? second : nil
            if replaceAccount {
                notifications.didRegister(deviceToken: Data([2]))
                notifications.set(.input, false)
            }
            await targets.releaseWrite()
            await notifications.pendingRegistration?.value
            let saved = await targets.snapshot()
            XCTAssertNil(saved["first"], "A completed old-account POST must not survive logout or replacement")
            if replaceAccount {
                XCTAssertEqual(saved, ["second": PushPrefs(done: false, input: false, failed: true)])
            } else {
                XCTAssertTrue(saved.isEmpty)
            }
        }
    }

    @MainActor
    func testNotificationPreferenceChangesLeaveOnlyTheLatestTarget() async {
        let suite = "notification-prefs-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let targets = NotificationTargets()
        let client = NotificationClient(owner: "owner", targets: targets)
        let notifications = PushNotifications(defaults: defaults, currentClient: { client })
        notifications.didRegister(deviceToken: Data([1]))
        await fulfillment(of: [targets.writeStarted], timeout: 2)
        notifications.enabled = false
        notifications.set(.done, false)
        notifications.enabled = true
        notifications.set(.input, false)
        await targets.releaseWrite()
        await notifications.pendingRegistration?.value
        let enabled = await targets.snapshot()
        XCTAssertEqual(enabled, ["owner": PushPrefs(done: false, input: false, failed: true)])
        notifications.enabled = false
        await notifications.pendingRegistration?.value
        let disabled = await targets.snapshot()
        XCTAssertTrue(disabled.isEmpty, "Turning notifications off removes the target after earlier writes")
    }

    func testExistingLinksUseTheSharedParser() {
        let id = "8b3ca06e-1fe0-4b72-bbe3-7bc7ec760baf"
        for scheme in ["comet", "comet-staging"] {
            XCTAssertTrue(crewIsSessionLink(url: "\(scheme)://session/project/\(id)?deploymentId=staging"))
            XCTAssertTrue(crewIsSessionLink(url: "\(scheme)://scaffold/project/staging/\(id)/sandbox"))
            XCTAssertFalse(crewIsSessionLink(url: "\(scheme)://session/project/\(id)?token=secret"))
        }
    }
}

private actor NotificationTargets {
    nonisolated let writeStarted = XCTestExpectation(description: "first target write in flight")
    private var holdFirstWrite = true
    private var heldWrite: CheckedContinuation<Void, Never>?
    private var targets: [String: PushPrefs] = [:]

    func upsert(owner: String, prefs: PushPrefs) async {
        if holdFirstWrite {
            holdFirstWrite = false
            await withCheckedContinuation { continuation in
                heldWrite = continuation
                writeStarted.fulfill()
            }
        }
        targets[owner] = prefs
    }

    func releaseWrite() {
        heldWrite?.resume()
        heldWrite = nil
    }

    func remove(owner: String) { targets.removeValue(forKey: owner) }
    func snapshot() -> [String: PushPrefs] { targets }
}

private final class NotificationClient: CoreClient, @unchecked Sendable {
    private let owner: String
    private let targets: NotificationTargets

    init(owner: String, targets: NotificationTargets) {
        self.owner = owner
        self.targets = targets
        super.init(noHandle: .init())
    }

    required init(unsafeFromHandle handle: UInt64) { fatalError("Use the notification fixture initializer") }
    override func isDemo() -> Bool { false }
    override func registerPushTarget(token: String, environment: String, prefs: PushPrefs) async throws {
        await targets.upsert(owner: owner, prefs: prefs)
    }
    override func unregisterPushTarget() async throws { await targets.remove(owner: owner) }
}
