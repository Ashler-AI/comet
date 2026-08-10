// Sign-in — Scaffold OAuth authorization code + dynamic native-client
// registration + PKCE S256. The issued revocable `sc_rc_` bearer is validated
// against Scaffold before it is stored.

import AuthenticationServices
import Network
import SwiftUI

/// Production cloud endpoints — mirrors edge/wrangler.jsonc.
enum Endpoints {
    static let edgeURL = URL(string: "https://comet.internal.ashler.com")!
    static let scaffoldURL = URL(string: "https://scaffold.internal.ashler.com")!
    static let projectScope = "ashler-production"
    static let loopbackHost = "127.0.0.1"
}

struct SignInView: View {
    @Environment(AppModel.self) private var model
    @State private var busy = false
    @State private var error: String?
    @State private var authSession = AuthSessionCoordinator()

    var body: some View {
        ZStack {
            Theme.bg.ignoresSafeArea()

            VStack(spacing: 32) {
                Spacer()

                VStack(spacing: 24) {
                    CrewMark()
                        .frame(width: 72, height: 72)
                    VStack(spacing: 6) {
                        Text("Comet")
                            .font(Theme.sans(28, weight: .semibold))
                            .kerning(-0.5)
                            .foregroundStyle(Theme.text)
                        Text("Your coding agents, from anywhere")
                            .font(Theme.sans(15))
                            .foregroundStyle(Theme.textMuted)
                    }
                }

                VStack(spacing: 12) {
                    Button {
                        signIn()
                    } label: {
                        Group {
                            if busy {
                                ProgressView()
                                    .tint(Theme.bg)
                            } else {
                                Text("Log in to Comet")
                                    .font(Theme.sans(15, weight: .semibold))
                                    .foregroundStyle(Theme.bg)
                            }
                        }
                        .frame(maxWidth: .infinity)
                        .frame(height: 50)
                        .background(Theme.text, in: RoundedRectangle(cornerRadius: 16))
                    }
                    .buttonStyle(.plain)
                    .disabled(busy)
                    .opacity(busy ? 0.6 : 1)

                    if let error {
                        Text(error)
                            .font(Theme.sans(13))
                            .foregroundStyle(Theme.danger)
                            .multilineTextAlignment(.center)
                    }
                }

                Spacer()
            }
            .padding(.horizontal, 32)
            .frame(maxWidth: 480)
        }
    }

    private func signIn() {
        busy = true
        error = nil
        authSession.start(
            begin: { redirectURI in
                try await model.beginSignIn(
                    scaffoldURL: Endpoints.scaffoldURL,
                    redirectURI: redirectURI
                )
            }
        ) { result in
            Task { @MainActor in
                switch result {
                case .cancelled:
                    break
                case .failure(let message):
                    error = message
                case .success(let flow, let callbackURL):
                    do {
                        try await model.completeSignIn(
                            edgeURL: Endpoints.edgeURL,
                            scaffoldURL: Endpoints.scaffoldURL,
                            projectScope: Endpoints.projectScope,
                            flow: flow,
                            callbackURL: callbackURL
                        )
                    } catch {
                        self.error = error.localizedDescription
                    }
                }
                busy = false
            }
        }
    }
}

// MARK: - Auth session plumbing

/// Runs OAuth in the system sheet while an HTTP listener bound only to
/// 127.0.0.1 receives Scaffold's exact loopback redirect.
@MainActor
final class AuthSessionCoordinator: NSObject, ASWebAuthenticationPresentationContextProviding {
    enum Outcome {
        case success(OAuthFlow, URL)
        case cancelled
        case failure(String)
    }

    private var listener: NWListener?
    private var session: ASWebAuthenticationSession?
    private var flow: OAuthFlow?
    private var completion: ((Outcome) -> Void)?
    private var beginning = false
    private var finished = false
    private let networkQueue = DispatchQueue(label: "dev.cometnative.Comet.oauth-loopback")

    func start(
        begin: @MainActor @escaping (String) async throws -> OAuthFlow,
        completion: @escaping (Outcome) -> Void
    ) {
        guard listener == nil, session == nil else {
            completion(.failure("A sign-in is already in progress"))
            return
        }
        self.completion = completion
        finished = false
        beginning = false
        do {
            let parameters = NWParameters.tcp
            parameters.requiredLocalEndpoint = .hostPort(
                host: NWEndpoint.Host(Endpoints.loopbackHost),
                port: .any
            )
            let listener = try NWListener(using: parameters)
            self.listener = listener
            listener.stateUpdateHandler = { [weak self] state in
                Task { @MainActor in
                    self?.handleListenerState(state, begin: begin)
                }
            }
            listener.newConnectionHandler = { [weak self] connection in
                Task { @MainActor in
                    self?.accept(connection)
                }
            }
            listener.start(queue: networkQueue)
        } catch {
            finish(.failure(error.localizedDescription))
        }
    }

    private func handleListenerState(
        _ state: NWListener.State,
        begin: @MainActor @escaping (String) async throws -> OAuthFlow
    ) {
        switch state {
        case .ready:
            guard !beginning, let port = listener?.port else { return }
            beginning = true
            let redirectURI = "http://\(Endpoints.loopbackHost):\(port.rawValue)/callback"
            Task {
                do {
                    let flow = try await begin(redirectURI)
                    guard !finished else { return }
                    self.flow = flow
                    let session = ASWebAuthenticationSession(
                        url: flow.authorizeURL,
                        callbackURLScheme: nil
                    ) { [weak self] _, error in
                        Task { @MainActor in
                            guard let self, !self.finished else { return }
                            if let error = error as? ASWebAuthenticationSessionError,
                               error.code == .canceledLogin {
                                self.finish(.cancelled)
                            } else {
                                self.finish(.failure(error?.localizedDescription ?? "Sign-in failed"))
                            }
                        }
                    }
                    session.presentationContextProvider = self
                    session.prefersEphemeralWebBrowserSession = false
                    self.session = session
                    if !session.start() {
                        finish(.failure("Could not open the sign-in browser"))
                    }
                } catch {
                    finish(.failure(error.localizedDescription))
                }
            }
        case .failed(let error):
            finish(.failure("Could not start the OAuth callback listener: \(error)"))
        case .cancelled:
            if !finished { finish(.failure("OAuth callback listener stopped")) }
        default:
            break
        }
    }

    private func accept(_ connection: NWConnection) {
        connection.start(queue: networkQueue)
        receive(connection, buffer: Data())
    }

    private func receive(_ connection: NWConnection, buffer: Data) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 16_384) {
            [weak self] data, _, complete, error in
            Task { @MainActor in
                guard let self else { return }
                var request = buffer
                if let data { request.append(data) }
                if request.range(of: Data("\r\n\r\n".utf8)) != nil {
                    self.handleRequest(request, connection: connection)
                } else if error == nil, !complete, request.count < 32_768 {
                    self.receive(connection, buffer: request)
                } else {
                    self.respond(status: "400 Bad Request", body: "Invalid OAuth callback.",
                                 connection: connection, then: nil)
                }
            }
        }
    }

    private func handleRequest(_ data: Data, connection: NWConnection) {
        guard let request = String(data: data, encoding: .utf8),
              let requestLine = request.components(separatedBy: "\r\n").first else {
            respond(status: "400 Bad Request", body: "Invalid OAuth callback.",
                    connection: connection, then: nil)
            return
        }
        let fields = requestLine.split(separator: " ")
        guard fields.count == 3, fields[0] == "GET",
              let port = listener?.port,
              let callback = URL(
                string: String(fields[1]),
                relativeTo: URL(string: "http://\(Endpoints.loopbackHost):\(port.rawValue)")!
              )?.absoluteURL,
              callback.path == "/callback",
              let flow else {
            respond(status: "404 Not Found", body: "Not found.",
                    connection: connection, then: nil)
            return
        }
        respond(
            status: "200 OK",
            body: "Sign-in complete. You can return to Comet.",
            connection: connection,
            then: { [weak self] in self?.finish(.success(flow, callback)) }
        )
    }

    private func respond(
        status: String,
        body: String,
        connection: NWConnection,
        then completion: (() -> Void)?
    ) {
        let payload = Data(body.utf8)
        let response = Data(
            ("HTTP/1.1 \(status)\r\nContent-Type: text/plain; charset=utf-8\r\n"
                + "Content-Length: \(payload.count)\r\nConnection: close\r\n\r\n").utf8
        ) + payload
        connection.send(content: response, completion: .contentProcessed { _ in
            connection.cancel()
            Task { @MainActor in completion?() }
        })
    }

    private func finish(_ outcome: Outcome) {
        guard !finished else { return }
        finished = true
        listener?.cancel()
        session?.cancel()
        listener = nil
        session = nil
        flow = nil
        let completion = self.completion
        self.completion = nil
        completion?(outcome)
    }

    nonisolated func presentationAnchor(
        for session: ASWebAuthenticationSession
    ) -> ASPresentationAnchor {
        MainActor.assumeIsolated {
            let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
            if let window = scenes.compactMap(\.keyWindow).first {
                return window
            }
            guard let scene = scenes.first else {
                preconditionFailure("OAuth presentation requires a window scene")
            }
            return ASPresentationAnchor(windowScene: scene)
        }
    }
}


/// The Crew mark from `assets/brand/crew-mark.svg`, rendered natively so the
/// stones follow the current foreground while the live core keeps its brand green.
struct CrewMark: View {
    var color: Color = Theme.text

    var body: some View {
        ZStack {
            CrewStonesShape().fill(color)
            CrewCoreShape().fill(Theme.crewCore)
        }
        .aspectRatio(1, contentMode: .fit)
    }
}

private struct CrewStonesShape: Shape {
    func path(in rect: CGRect) -> Path {
        let scale = min(rect.width, rect.height) / 46
        let dx = rect.minX + (rect.width - 46 * scale) / 2
        let dy = rect.minY + (rect.height - 46 * scale) / 2
        let radius = CGSize(width: 1.2 * scale, height: 1.2 * scale)
        let stones: [CGRect] = [
            CGRect(x: dx, y: dy, width: 34 * scale, height: 10 * scale),
            CGRect(x: dx + 36 * scale, y: dy, width: 10 * scale, height: 34 * scale),
            CGRect(x: dx + 12 * scale, y: dy + 36 * scale, width: 34 * scale, height: 10 * scale),
            CGRect(x: dx, y: dy + 12 * scale, width: 10 * scale, height: 34 * scale),
        ]

        var path = Path()
        for stone in stones {
            path.addRoundedRect(in: stone, cornerSize: radius)
        }
        return path
    }
}

private struct CrewCoreShape: Shape {
    func path(in rect: CGRect) -> Path {
        let scale = min(rect.width, rect.height) / 46
        let dx = rect.minX + (rect.width - 46 * scale) / 2
        let dy = rect.minY + (rect.height - 46 * scale) / 2
        let core = CGRect(
            x: dx + 18 * scale,
            y: dy + 18 * scale,
            width: 10 * scale,
            height: 10 * scale
        )
        var path = Path()
        path.addRoundedRect(
            in: core,
            cornerSize: CGSize(width: 1.2 * scale, height: 1.2 * scale)
        )
        return path
    }
}
