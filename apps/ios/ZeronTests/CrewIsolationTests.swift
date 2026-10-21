import XCTest
@testable import Zeron

final class CrewIsolationTests: XCTestCase {
    @MainActor
    func testCreatingSandboxNeverReusesSelectedSessionIdentity() throws {
        let selected = UUID().uuidString.lowercased()
        let created = try CrewControlsViewController.sessionID(for: "create", selected: selected)
        XCTAssertNotEqual(created, selected)
        XCTAssertNotNil(UUID(uuidString: created))
        XCTAssertNotEqual(created, try CrewControlsViewController.sessionID(for: "create", selected: selected))
        XCTAssertEqual(try CrewControlsViewController.sessionID(for: "attach", selected: selected), selected)
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

    func testExistingAndCanaryLinksUseTheSharedParser() {
        let id = "8b3ca06e-1fe0-4b72-bbe3-7bc7ec760baf"
        for scheme in ["comet", "comet-staging", "crew-upstream-canary"] {
            XCTAssertTrue(crewIsSessionLink(url: "\(scheme)://session/project/\(id)?deploymentId=staging"))
            XCTAssertTrue(crewIsSessionLink(url: "\(scheme)://scaffold/project/staging/\(id)/sandbox"))
            XCTAssertFalse(crewIsSessionLink(url: "\(scheme)://session/project/\(id)?token=secret"))
        }
    }
}
