// Session-wide connection config: edge base URL, identity, token minting for
// room sockets (WS auth rides the URL query — sockets can't set headers), and
// the durable-nudge POST. Thread-safe (rooms call in from their actors).

import Foundation

enum ReleaseConfig {
    static let edgeURL = requiredURL("CrewEdgeURL")
    static let scaffoldURL = requiredURL("CrewScaffoldURL")
    static let projectScope = requiredString("CrewProjectScope")
    static let inviteScheme = requiredString("CrewInviteScheme")
    static let displayName = requiredString("CFBundleDisplayName")

    private static func requiredString(_ key: String) -> String {
        guard let value = Bundle.main.object(forInfoDictionaryKey: key) as? String,
              !value.isEmpty else {
            fatalError("Missing required Info.plist value: \(key)")
        }
        return value
    }

    private static func requiredURL(_ key: String) -> URL {
        let value = requiredString(key)
        guard let url = URL(string: value), url.scheme == "https", url.host != nil else {
            fatalError("Invalid required URL in Info.plist: \(key)")
        }
        return url
    }
}

final class AppConfig: @unchecked Sendable {
    enum Mode: String {
        case scaffold
        case dev
    }

    let edgeURL: URL
    let mode: Mode
    let userId: String
    let projectScope: String
    let deviceId: String
    let deviceName: String

    private let tokens: AuthTokens?
    private let devBearer: String?

    init(edgeURL: URL, mode: Mode, userId: String, projectScope: String,
         deviceId: String, deviceName: String,
         tokens: AuthTokens? = nil, devBearer: String? = nil) {
        self.edgeURL = edgeURL
        self.mode = mode
        self.userId = userId
        self.projectScope = projectScope
        self.deviceId = deviceId
        self.deviceName = deviceName
        self.tokens = tokens
        self.devBearer = devBearer
    }

    /// Current revocable Scaffold bearer. The control plane validates it on
    /// every edge request, so no client-side refresh token exists.
    func currentToken() async -> String? {
        switch mode {
        case .dev: return devBearer
        case .scaffold: return tokens?.accessToken
        }
    }

    private var wsBase: URL {
        var components = URLComponents(url: edgeURL, resolvingAgainstBaseURL: false)!
        components.scheme = components.scheme == "http" ? "ws" : "wss"
        return components.url!
    }

    func workspaceSocketURL() async -> URL? {
        guard let token = await currentToken() else { return nil }
        var url = wsBase.appending(path: "workspace/\(projectScope)/ws")
        url.append(queryItems: [URLQueryItem(name: "token", value: token)])
        return url
    }

    func sessionSocketURL(chatId: String, deploymentId: String? = nil) async -> URL? {
        guard let token = await currentToken() else { return nil }
        var url = wsBase.appending(path: "session/\(chatId)/ws")
        url.append(queryItems: [
            URLQueryItem(name: "token", value: token),
            URLQueryItem(name: "deploymentId", value: deploymentId ?? projectScope),
        ])
        return url
    }


    /// GET /device/{deviceId}/status → whether the device's relay HOST socket
    /// is currently attached (distinct from workspace presence).
    func deviceStatus(deviceId: String) async -> String {
        guard let token = await currentToken() else { return "no-token" }
        var request = URLRequest(url: edgeURL.appending(path: "device/\(deviceId)/status"))
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        guard let (data, response) = try? await URLSession.shared.data(for: request),
              let http = response as? HTTPURLResponse else { return "unreachable" }
        return "http=\(http.statusCode) body=\(String(data: data, encoding: .utf8) ?? "")"
    }

    /// POST /device/{deviceId}/nudge {chatId} — wake a cold host to drain the
    /// command queue.
    func nudge(deviceId: String, chatId: String) async {
        guard let token = await currentToken() else { return }
        var request = URLRequest(url: edgeURL.appending(path: "device/\(deviceId)/nudge"))
        request.httpMethod = "POST"
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try? JSONSerialization.data(withJSONObject: ["chatId": chatId])
        _ = try? await URLSession.shared.data(for: request)
    }
}
