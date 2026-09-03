// Scaffold OAuth client. Discovers the protected resource and authorization
// server, dynamically registers this native public client, uses PKCE S256, and
// validates the issued remote-code bearer before Comet stores it.

import CryptoKit
import Foundation
import Security

struct AuthUser: Codable, Equatable {
    var id: String
    var email: String
    var name: String?
}

struct AuthTokens: Codable, Equatable {
    var accessToken: String
}

struct OAuthFlow: Equatable {
    var authorizeURL: URL
    var state: String
    var clientId: String
    var redirectURI: String
    var codeVerifier: String
    var tokenEndpoint: URL
    var resource: URL
}

enum AuthError: LocalizedError {
    case http(Int, String)
    case invalidResponse
    case invalidContract(String)

    var errorDescription: String? {
        switch self {
        case .http(let code, let body): return "Auth failed (\(code)): \(body)"
        case .invalidResponse: return "Unexpected auth response"
        case .invalidContract(let message): return message
        }
    }
}

struct AuthClient {
    private static let scopes = [
        "remote_code:create",
        "remote_code:read",
        "remote_code:write",
        "remote_code:exec",
        "remote_code:lifecycle",
    ]

    var scaffoldURL: URL

    func beginSignIn(redirectURI: String) async throws -> OAuthFlow {
        struct ProtectedResource: Decodable {
            var resource: String
            var authorizationServers: [String]
            var scopesSupported: [String]

        }
        struct AuthorizationServer: Decodable {
            var issuer: String
            var authorizationEndpoint: String
            var tokenEndpoint: String
            var registrationEndpoint: String
            var codeChallengeMethodsSupported: [String]

        }
        struct Registration: Decodable { var clientId: String }

        let protected: ProtectedResource = try await get(
            scaffoldURL.appending(path: ".well-known/oauth-protected-resource")
        )
        let resource = try sameOriginURL(protected.resource, label: "OAuth resource")
        guard Self.scopes.allSatisfy(Set(protected.scopesSupported).contains) else {
            throw AuthError.invalidContract("Scaffold does not advertise the required remote-code scopes")
        }
        guard let issuerString = protected.authorizationServers.first else {
            throw AuthError.invalidContract("OAuth metadata has no authorization server")
        }
        let issuer = try sameOriginURL(issuerString, label: "OAuth issuer")
        let server: AuthorizationServer = try await get(
            issuer.appending(path: ".well-known/oauth-authorization-server")
        )
        _ = try sameOriginURL(server.issuer, label: "OAuth issuer")
        let authorizationEndpoint = try sameOriginURL(
            server.authorizationEndpoint, label: "OAuth authorization endpoint"
        )
        let tokenEndpoint = try sameOriginURL(server.tokenEndpoint, label: "OAuth token endpoint")
        let registrationEndpoint = try sameOriginURL(
            server.registrationEndpoint, label: "OAuth registration endpoint"
        )
        guard server.codeChallengeMethodsSupported.contains("S256") else {
            throw AuthError.invalidContract("OAuth server does not support PKCE S256")
        }

        let registration: Registration = try await postJSON(registrationEndpoint, body: [
            "redirect_uris": [redirectURI],
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
        ])
        guard !registration.clientId.isEmpty else {
            throw AuthError.invalidContract("OAuth registration returned no client id")
        }

        let verifier = try randomVerifier()
        let challenge = Self.base64URL(Data(SHA256.hash(data: Data(verifier.utf8))))
        let state = UUID().uuidString.lowercased()
        var authorize = URLComponents(url: authorizationEndpoint, resolvingAgainstBaseURL: false)!
        authorize.queryItems = [
            URLQueryItem(name: "response_type", value: "code"),
            URLQueryItem(name: "client_id", value: registration.clientId),
            URLQueryItem(name: "redirect_uri", value: redirectURI),
            URLQueryItem(name: "state", value: state),
            URLQueryItem(name: "code_challenge", value: challenge),
            URLQueryItem(name: "code_challenge_method", value: "S256"),
            URLQueryItem(name: "resource", value: resource.absoluteString),
            URLQueryItem(name: "scope", value: Self.scopes.joined(separator: " ")),
        ]
        guard let authorizeURL = authorize.url else { throw AuthError.invalidResponse }
        return OAuthFlow(
            authorizeURL: authorizeURL,
            state: state,
            clientId: registration.clientId,
            redirectURI: redirectURI,
            codeVerifier: verifier,
            tokenEndpoint: tokenEndpoint,
            resource: resource
        )
    }

    func completeSignIn(flow: OAuthFlow, callbackURL: URL) async throws -> (AuthUser, AuthTokens) {
        struct Tokens: Decodable {
            var accessToken: String
            var tokenType: String
            var scope: String
            var resource: String
        }
        struct Actor: Decodable {
            var sub: String
            var auth: String
            var displayName: String?
        }
        struct Session: Decodable {
            var ok: Bool
            var resource: String
            var actor: Actor
            var scopes: [String]
        }

        let query = URLComponents(url: callbackURL, resolvingAgainstBaseURL: false)?.queryItems ?? []
        let value = { (name: String) in query.first { $0.name == name }?.value }
        guard value("state") == flow.state, let code = value("code"), !code.isEmpty else {
            throw AuthError.invalidContract("Callback missing code or state mismatch")
        }
        let tokens: Tokens = try await postForm(flow.tokenEndpoint, fields: [
            "grant_type": "authorization_code",
            "code": code,
            "client_id": flow.clientId,
            "redirect_uri": flow.redirectURI,
            "code_verifier": flow.codeVerifier,
            "resource": flow.resource.absoluteString,
        ])
        let granted = Set(tokens.scope.split(whereSeparator: \.isWhitespace).map(String.init))
        guard tokens.tokenType == "Bearer",
              tokens.accessToken.hasPrefix("sc_rc_"),
              try sameOriginURL(tokens.resource, label: "token resource") == flow.resource,
              Self.scopes.allSatisfy(granted.contains) else {
            throw AuthError.invalidContract("OAuth token response violated the Scaffold contract")
        }

        var request = URLRequest(
            url: flow.resource.appending(path: "api/code-sandboxes/auth/session")
        )
        request.setValue("Bearer \(tokens.accessToken)", forHTTPHeaderField: "Authorization")
        let session: Session = try await send(request)
        let sessionScopes = Set(session.scopes)
        guard session.ok,
              session.actor.auth == "iap",
              !session.actor.sub.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              try sameOriginURL(session.resource, label: "identity resource") == flow.resource,
              Self.scopes.allSatisfy(sessionScopes.contains) else {
            throw AuthError.invalidContract("Scaffold identity response was not authorized")
        }
        let subject = session.actor.sub.lowercased()
        return (
            AuthUser(id: subject, email: subject, name: session.actor.displayName),
            AuthTokens(accessToken: tokens.accessToken)
        )
    }

    private func sameOriginURL(_ value: String, label: String) throws -> URL {
        guard let url = URL(string: value),
              url.scheme == scaffoldURL.scheme,
              url.host?.lowercased() == scaffoldURL.host?.lowercased(),
              url.port == scaffoldURL.port else {
            throw AuthError.invalidContract("\(label) has an untrusted origin")
        }
        return url
    }

    private func randomVerifier() throws -> String {
        var bytes = [UInt8](repeating: 0, count: 32)
        guard SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess else {
            throw AuthError.invalidContract("Could not generate OAuth verifier")
        }
        return Self.base64URL(Data(bytes))
    }

    private static func base64URL(_ data: Data) -> String {
        data.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }

    private func get<T: Decodable>(_ url: URL) async throws -> T {
        try await send(URLRequest(url: url))
    }

    private func postJSON<T: Decodable>(_ url: URL, body: [String: Any]) async throws -> T {
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONSerialization.data(withJSONObject: body)
        return try await send(request)
    }

    private func postForm<T: Decodable>(_ url: URL, fields: [String: String]) async throws -> T {
        var components = URLComponents()
        components.queryItems = fields.sorted { $0.key < $1.key }.map {
            URLQueryItem(name: $0.key, value: $0.value)
        }
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("application/x-www-form-urlencoded", forHTTPHeaderField: "Content-Type")
        request.httpBody = components.percentEncodedQuery?.data(using: .utf8)
        return try await send(request)
    }

    private func send<T: Decodable>(_ request: URLRequest) async throws -> T {
        let (data, response) = try await URLSession.shared.data(for: request)
        guard let http = response as? HTTPURLResponse else { throw AuthError.invalidResponse }
        guard (200..<300).contains(http.statusCode) else {
            throw AuthError.http(http.statusCode, String(data: data, encoding: .utf8) ?? "")
        }
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(T.self, from: data)
    }
}

// MARK: - Keychain storage

enum Keychain {
    private static let service = Bundle.main.bundleIdentifier ?? "ai.ashler.crew"

    static func save(_ value: String, key: String) {
        let data = Data(value.utf8)
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: key,
        ]
        SecItemDelete(query as CFDictionary)
        var add = query
        add[kSecValueData as String] = data
        add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
        SecItemAdd(add as CFDictionary, nil)
    }

    static func load(key: String) -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: key,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: AnyObject?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess,
              let data = result as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func delete(key: String) {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: key,
        ]
        SecItemDelete(query as CFDictionary)
    }
}
