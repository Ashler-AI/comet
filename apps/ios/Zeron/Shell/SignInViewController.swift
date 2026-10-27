import AuthenticationServices
import CryptoKit
import UIKit

enum Endpoints {
    static let callbackScheme = "comet-staging"
    static func setting(_ key: String) throws -> String {
        guard let value = Bundle.main.object(forInfoDictionaryKey: key) as? String,
              !value.isEmpty, !value.contains("$(") else {
            throw CoreError.Auth(message: "Configure \(key) for the Crew Staging build.")
        }
        return value
    }
    static var edgeURL: URL? { (try? setting("CrewEdgeOrigin")).flatMap(URL.init(string:)) }
    static var deploymentId: String? { try? setting("CrewDeploymentId") }
    static var storageScope: String {
        let fields = ["CrewControlOrigin", "CrewEdgeOrigin", "CrewProjectScope", "CrewDeploymentId"]
            .map { (try? setting($0)) ?? "unconfigured" }
        let encoded = (try? JSONEncoder().encode(fields)) ?? Data()
        return SHA256.hash(data: encoded).map { String(format: "%02x", $0) }.joined()
    }
    static func login() throws -> CrewLogin {
        try CrewLogin(controlOrigin: setting("CrewControlOrigin"),
                      edgeOrigin: setting("CrewEdgeOrigin"),
                      projectScope: setting("CrewProjectScope"),
                      deploymentId: setting("CrewDeploymentId"))
    }
}

/// Resource-bound Scaffold OAuth, shared with the host through the Rust core.
final class SignInViewController: UIViewController, ASWebAuthenticationPresentationContextProviding {
    private let app: AppModel
    private let status = UILabel()
    private let signIn = UIButton(type: .system)
    private var session: ASWebAuthenticationSession?
    private var login: CrewLogin?
    private var appToken: AnyObject?

    init(app: AppModel) {
        self.app = app
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = Palette.background
        let mark = UIImageView(image: UIImage(systemName: "sparkle", withConfiguration: UIImage.SymbolConfiguration(pointSize: 44, weight: .light)))
        mark.tintColor = Palette.text
        let title = UILabel()
        title.text = "Crew Staging"
        title.font = Fonts.ui(.sansSemibold, 34)
        title.textColor = Palette.text
        let tagline = UILabel()
        tagline.text = "Your coding agents, from anywhere."
        tagline.font = Fonts.ui(.sans, 17)
        tagline.textColor = Palette.secondary

        var primary = UIButton.Configuration.filled()
        primary.title = "Sign In"
        primary.baseBackgroundColor = Palette.text
        primary.baseForegroundColor = Palette.background
        primary.cornerStyle = .capsule
        primary.contentInsets = NSDirectionalEdgeInsets(top: 15, leading: 20, bottom: 15, trailing: 20)
        primary.titleTextAttributesTransformer = UIConfigurationTextAttributesTransformer { a in
            var a = a
            a.font = Fonts.ui(.sansSemibold, 17)
            a.foregroundColor = Palette.background
            return a
        }
        signIn.configuration = primary
        signIn.accessibilityIdentifier = "sign-in"
        signIn.addAction(UIAction { [weak self] _ in self?.startCrewLogin() }, for: .touchUpInside)

        status.font = Fonts.ui(.sans, 14)
        status.textColor = Palette.danger
        status.numberOfLines = 0
        status.textAlignment = .center
        status.text = app.restoreError
        appToken = app.observe { [weak self] in self?.status.text = self?.app.restoreError }

        let top = UIStackView(arrangedSubviews: [mark, title, tagline])
        top.axis = .vertical
        top.alignment = .center
        top.spacing = 12
        let buttons = UIStackView(arrangedSubviews: [status, signIn])
        buttons.axis = .vertical
        buttons.spacing = 12
        for v in [top, buttons] {
            v.translatesAutoresizingMaskIntoConstraints = false
            view.addSubview(v)
        }
        NSLayoutConstraint.activate([
            top.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            top.centerYAnchor.constraint(equalTo: view.centerYAnchor, constant: -80),
            buttons.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor, constant: 24),
            buttons.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor, constant: -24),
            buttons.bottomAnchor.constraint(equalTo: view.safeAreaLayoutGuide.bottomAnchor, constant: -24),
        ])
    }

    private func startCrewLogin() {
        app.cancelPendingRestore()
        signIn.isEnabled = false
        signIn.configuration?.showsActivityIndicator = true
        status.text = nil
        Task { @MainActor in
            do {
                let login = try Endpoints.login()
                self.login = login
                let authorize = try await login.begin(redirectUri: "\(Endpoints.callbackScheme)://callback")
                guard let url = URL(string: authorize) else { throw CoreError.Auth(message: "Invalid authorization URL") }
                let session = ASWebAuthenticationSession(url: url, callback: .customScheme(Endpoints.callbackScheme)) { [weak self] callback, error in
                    Task { @MainActor in
                        guard let self else { return }
                        defer {
                            self.signIn.isEnabled = true
                            self.signIn.configuration?.showsActivityIndicator = false
                            self.login = nil
                        }
                        guard let callback else {
                            login.cancel()
                            if (error as? ASWebAuthenticationSessionError)?.code != .canceledLogin {
                                self.status.text = error?.localizedDescription ?? "Sign-in did not complete."
                            }
                            return
                        }
                        do { try await self.app.signIn(account: login.complete(callback: callback.absoluteString)) }
                        catch { self.status.text = error.localizedDescription }
                    }
                }
                session.presentationContextProvider = self
                session.prefersEphemeralWebBrowserSession = false
                self.session = session
                if !session.start() { throw CoreError.Auth(message: "Could not open the sign-in sheet.") }
            } catch {
                login?.cancel()
                status.text = error.localizedDescription
                signIn.isEnabled = true
                signIn.configuration?.showsActivityIndicator = false
            }
        }
    }

    func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        view.window ?? ASPresentationAnchor()
    }
}
