import XCTest
@testable import Zeron

final class CrewIsolationTests: XCTestCase {
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
