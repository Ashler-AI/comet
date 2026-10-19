import UIKit

/// All operations go through a verified desktop controller; the phone never owns execution credentials.
final class CrewControlsViewController: UIViewController {
    private let app: AppModel
    private let stack = UIStackView()
    private let status = UILabel()
    private let hostButton = UIButton(type: .system)
    private let sandboxButton = UIButton(type: .system)
    private let modelButton = UIButton(type: .system)
    private let advancedButton = UIButton(type: .system)
    private let advanced = UIStackView()
    private let session = UITextField()
    private let sandbox = UITextField()
    private let model = UITextField()
    private let account = UITextField()
    private let database = UISegmentedControl(items: ["Local DB", "Staging snapshot"])
    private var hostId: String?
    private var busy = false
    private var records: [Command] = []
    private let storageURL: URL

    struct Command: Codable {
        let id: String
        let deviceId: String
        let controlJSON: String
        var status: String
        var result: String?
    }

    init(app: AppModel) {
        self.app = app
        self.storageURL = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("crew-canary-profiles", isDirectory: true)
            .appendingPathComponent(app.crewStorageKey ?? "signed-out", isDirectory: true)
            .appendingPathComponent("scaffold-commands.json")
        super.init(nibName: nil, bundle: nil)
        title = "Scaffold sandboxes"
    }
    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = Palette.background
        let scroll = UIScrollView()
        scroll.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(scroll)
        stack.axis = .vertical
        stack.spacing = 16
        stack.translatesAutoresizingMaskIntoConstraints = false
        scroll.addSubview(stack)
        NSLayoutConstraint.activate([
            scroll.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor),
            scroll.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor),
            scroll.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor, constant: 20),
            stack.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor, constant: -20),
            stack.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor, constant: 20),
            stack.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor, constant: -20),
            stack.widthAnchor.constraint(equalTo: scroll.frameLayoutGuide.widthAnchor, constant: -40),
        ])
        hostButton.setTitle("Choose desktop controller", for: .normal)
        hostButton.accessibilityIdentifier = "crew-control-host"
        hostButton.addAction(UIAction { [weak self] _ in self?.chooseHost() }, for: .touchUpInside)
        stack.addArrangedSubview(hostButton)
        status.numberOfLines = 0
        status.accessibilityIdentifier = "crew-control-status"
        stack.addArrangedSubview(status)
        addButton("Reconcile retained command") { [weak self] in self?.chooseCommand() }
        addHeading("Existing sandbox")
        sandboxButton.setTitle("Choose sandbox", for: .normal)
        sandboxButton.titleLabel?.numberOfLines = 0
        sandboxButton.accessibilityIdentifier = "crew-control-sandbox"
        sandboxButton.addAction(UIAction { [weak self] _ in self?.listSandboxes() }, for: .touchUpInside)
        stack.addArrangedSubview(sandboxButton)
        addButton("Open history") { [weak self] in self?.browse() }
        addButton("Use sandbox") { [weak self] in self?.confirm("attach") }
        let manageButton = UIButton(type: .system)
        manageButton.setTitle("Manage sandbox", for: .normal)
        manageButton.showsMenuAsPrimaryAction = true
        manageButton.menu = UIMenu(children: [("inspect", "Inspect"), ("pause", "Pause"), ("resume", "Resume"), ("stop", "Stop")].map { operation, title in
            UIAction(title: title, attributes: operation == "stop" ? .destructive : []) { [weak self] _ in
                self?.confirm(operation)
            }
        })
        stack.addArrangedSubview(manageButton)

        addHeading("Create sandbox")
        modelButton.setTitle("Choose model", for: .normal)
        modelButton.showsMenuAsPrimaryAction = true
        modelButton.menu = modelMenu()
        modelButton.accessibilityIdentifier = "crew-control-model"
        stack.addArrangedSubview(modelButton)
        for (field, label) in [(model, "Provider/model override"), (session, "Existing session UUID"), (sandbox, "Existing sandbox ID"), (account, "Optional opaque Agent Auth account ID")] {
            field.placeholder = label
            field.accessibilityLabel = label
            field.borderStyle = .roundedRect
            field.autocapitalizationType = .none
            field.autocorrectionType = .no
            advanced.addArrangedSubview(field)
        }
        sandbox.addAction(UIAction { [weak self] _ in
            guard let self else { return }
            self.sandboxButton.setTitle(self.sandbox.text?.nonEmpty.map { "Selected sandbox: \($0)" } ?? "Choose sandbox", for: .normal)
        }, for: .editingChanged)
        model.addAction(UIAction { [weak self] _ in
            guard let self else { return }
            self.modelButton.setTitle(self.model.text?.nonEmpty ?? "Choose model", for: .normal)
        }, for: .editingChanged)
        database.selectedSegmentIndex = 0
        database.accessibilityLabel = "Database environment"
        stack.addArrangedSubview(database)
        addButton("Create sandbox") { [weak self] in self?.confirm("create") }

        advanced.axis = .vertical
        advanced.spacing = 12
        advanced.isHidden = true
        advancedButton.setTitle("Advanced options", for: .normal)
        advancedButton.setImage(UIImage(systemName: "chevron.right"), for: .normal)
        advancedButton.accessibilityValue = "Collapsed"
        advancedButton.accessibilityIdentifier = "crew-control-advanced"
        advancedButton.addAction(UIAction { [weak self] _ in
            guard let self else { return }
            self.advanced.isHidden.toggle()
            self.advancedButton.setImage(UIImage(systemName: self.advanced.isHidden ? "chevron.right" : "chevron.down"), for: .normal)
            self.advancedButton.accessibilityValue = self.advanced.isHidden ? "Collapsed" : "Expanded"
        }, for: .touchUpInside)
        stack.addArrangedSubview(advancedButton)
        stack.addArrangedSubview(advanced)
        let explanation = UILabel()
        explanation.numberOfLines = 0
        explanation.font = .preferredFont(forTextStyle: .footnote)
        explanation.adjustsFontForContentSizeCategory = true
        explanation.text = "Project: \(app.client?.orgId() ?? "")\nDeployment: \(Endpoints.deploymentId ?? "not configured")\nOpen history does not resume a sandbox. Use sandbox attaches to or resumes the same sandbox; Stop is terminal. Stop a single agent turn from its composer instead. Source and region use deployment defaults. Inference always uses host Agent Auth."
        stack.addArrangedSubview(explanation)
        do {
            if FileManager.default.fileExists(atPath: storageURL.path) {
                records = try JSONDecoder().decode([Command].self, from: Data(contentsOf: storageURL))
            }
        } catch {
            busy = true
            status.text = "Retained commands could not be read. Original journal preserved; controls blocked until recovery. \(error.localizedDescription)"
        }
        if let last = records.last { status.text = "Retained command \(last.id): \(last.status). Reconcile before repeating an unknown operation." }
    }

    private func addHeading(_ title: String) {
        let label = UILabel()
        label.text = title
        label.font = .preferredFont(forTextStyle: .headline)
        label.adjustsFontForContentSizeCategory = true
        label.accessibilityTraits.insert(.header)
        stack.addArrangedSubview(label)
    }

    private func addButton(_ title: String, action: @escaping () -> Void) {
        let button = UIButton(type: .system)
        button.setTitle(title, for: .normal)
        button.addAction(UIAction { _ in action() }, for: .touchUpInside)
        stack.addArrangedSubview(button)
    }
    private func sheet(_ title: String, message: String? = nil) -> UIAlertController {
        let sheet = UIAlertController(title: title, message: message, preferredStyle: .actionSheet)
        sheet.popoverPresentationController?.sourceView = hostButton
        sheet.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        return sheet
    }
    private func chooseHost() {
        let menu = sheet("Desktop controller", message: "A sandbox or phone cannot authorize these operations.")
        for host in app.hostOptions where host.online {
            menu.addAction(UIAlertAction(title: host.name, style: .default) { [weak self] _ in
                guard let self else { return }
                if self.hostId != host.id {
                    self.model.text = nil
                    self.modelButton.setTitle("Choose model", for: .normal)
                    self.session.text = nil
                    self.sandbox.text = nil
                    self.sandboxButton.setTitle("Choose sandbox", for: .normal)
                }
                self.hostId = host.id
                self.hostButton.setTitle(host.name, for: .normal)
            })
        }
        present(menu, animated: true)
    }
    private func modelMenu() -> UIMenu {
        UIMenu(title: "Model", children: [UIDeferredMenuElement.uncached { [weak self] done in
            guard let self, let hostId = self.hostId, let client = self.app.client else {
                return done([UIAction(title: "Choose a desktop controller first", attributes: .disabled) { _ in }])
            }
            Task { @MainActor in
                let models = await client.listModels(deviceId: hostId, harness: "omp").filter {
                    let parts = $0.id.split(separator: "/", maxSplits: 1)
                    return parts.count == 2 && ["openai", "openai-codex", "anthropic"].contains(String(parts[0])) && !parts[1].isEmpty
                }
                guard self.hostId == hostId else { return done([]) }
                if models.isEmpty {
                    return done([UIAction(title: "No models available; refresh or use Advanced options", attributes: .disabled) { _ in }])
                }
                done(models.map { model in
                    UIAction(title: model.label, subtitle: model.id, state: model.id == self.model.text ? .on : .off) { [weak self] _ in
                        self?.model.text = model.id
                        self?.modelButton.setTitle(model.label, for: .normal)
                    }
                })
            }
        }])
    }
    private func listSandboxes() {
        guard !busy, let hostId, let client = app.client, let deployment = Endpoints.deploymentId else { status.text = "Select an online desktop controller."; return }
        busy = true
        Task { @MainActor in
            defer { busy = false }
            do {
                let raw = try await client.crewRefresh(deviceId: hostId, deployment: deployment)
                let value = try Self.json(raw)
                guard let environments = value["environments"] as? [[String: Any]] else { throw CoreError.HostError(message: "Invalid sandbox list") }
                let menu = sheet("Your sandboxes", message: environments.isEmpty ? "No sandboxes in this scope." : nil)
                for environment in environments {
                    guard let scope = environment["scope"] as? [String: Any], scope["projectId"] as? String == client.orgId(), scope["deploymentId"] as? String == deployment,
                          environment["ownerPrincipal"] as? String == client.userId(),
                          let source = environment["source"] as? [String: Any], let id = source["sandbox_id"] as? String,
                          let sessionId = scope["sessionId"] as? String else { continue }
                    let label = "\(environment["name"] as? String ?? id) · \(source["lifecycle"] as? String ?? "unknown")"
                    menu.addAction(UIAlertAction(title: label, style: .default) { [weak self] _ in
                        self?.sandbox.text = id; self?.session.text = sessionId
                        self?.sandboxButton.setTitle("Selected: \(label)", for: .normal)
                    })
                }
                status.text = nil
                present(menu, animated: true)
            } catch { status.text = error.localizedDescription }
        }
    }
    static func sessionID(for operation: String, selected: String?) throws -> String {
        if operation == "create" { return UUID().uuidString.lowercased() }
        guard let selected, UUID(uuidString: selected) != nil else {
            throw CoreError.InvalidArgument(message: "Choose an existing sandbox with a valid session UUID.")
        }
        return selected.lowercased()
    }

    private func control(_ operation: String, sessionId: String) throws -> String {
        guard let client = app.client, let deployment = Endpoints.deploymentId else {
            throw CoreError.InvalidArgument(message: "A configured deployment is required.")
        }
        var control: [String: Any] = ["operation": operation, "scope": ["projectId": client.orgId(), "deploymentId": deployment, "sessionId": sessionId]]
        if operation == "create" {
            let selected = model.text?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            let parts = selected.split(separator: "/", maxSplits: 1).map(String.init)
            guard parts.count == 2, ["openai", "openai-codex", "anthropic"].contains(parts[0]), !parts[1].isEmpty else {
                throw CoreError.InvalidArgument(message: "Choose a provider-qualified model (openai-codex/… or anthropic/…). Inference always uses host Agent Auth.")
            }
            var route: [String: Any] = ["provider": parts[0] == "anthropic" ? "anthropic" : "openai", "model": parts[1], "fallback": "disabled", "routingMode": "automatic"]
            if let accountId = account.text?.nonEmpty { route["accountId"] = accountId; route["routingMode"] = "pinned" }
            control["agentRoute"] = route
            control["database_environment"] = database.selectedSegmentIndex == 0 ? "local" : "staging_snapshot"
        } else {
            guard let id = sandbox.text?.nonEmpty else { throw CoreError.InvalidArgument(message: "Choose an existing sandbox.") }
            control["sandbox_id"] = id
        }
        return String(decoding: try JSONSerialization.data(withJSONObject: control), as: UTF8.self)
    }
    private func confirm(_ operation: String) {
        guard !busy, let hostId, let client = app.client, let deployment = Endpoints.deploymentId else { status.text = "Select an online desktop controller."; return }
        do {
            // Create allocates a distinct identity once per confirmation. The exact
            // payload is then journaled and reconciliation never regenerates it.
            let sessionId = try Self.sessionID(for: operation, selected: session.text)
            let control = try control(operation, sessionId: sessionId)
            let menu = sheet("\(operation.capitalized) this sandbox?", message: "Controller: \(hostButton.title(for: .normal) ?? hostId)\nSession: \(sessionId)\nSandbox: \(operation == "create" ? "new" : sandbox.text ?? "")\nDeployment: \(deployment)" + (operation == "create" ? "\nSource: Deployment default\nRegion: Deployment default\nModel: \(model.text ?? "")\nDB: \(database.titleForSegment(at: database.selectedSegmentIndex) ?? "")\nAccount: \(account.text?.nonEmpty ?? "Automatic host selection")" : ""))
            menu.addAction(UIAlertAction(title: operation.capitalized, style: operation == "stop" ? .destructive : .default) { [weak self] _ in
                guard let self else { return }
                let command = Command(id: UUID().uuidString.lowercased(), deviceId: hostId, controlJSON: control, status: "pending", result: nil)
                self.records.append(command)
                do {
                    try self.persist()
                    self.perform(command) { try await client.crewControl(deviceId: hostId, commandId: command.id, controlJson: control, deployment: deployment) }
                } catch {
                    self.records.removeAll { $0.id == command.id }
                    self.status.text = "Command was not sent: \(error.localizedDescription)"
                }
            })
            present(menu, animated: true)
        } catch { status.text = error.localizedDescription }
    }
    private func chooseCommand() {
        guard !busy, let client = app.client else { return }
        let menu = sheet("Reconcile; never replay", message: "Unknown operations keep their original command IDs.")
        for command in records.reversed() {
            menu.addAction(UIAlertAction(title: "\(command.status) · \(command.id)", style: .default) { [weak self] _ in
                self?.perform(command) { try await client.crewReconcile(deviceId: command.deviceId, commandId: command.id) }
            })
        }
        present(menu, animated: true)
    }
    private func perform(_ command: Command, action: @escaping () async throws -> String) {
        busy = true
        status.text = "Pending: \(command.id)"
        Task { @MainActor in
            defer {
                busy = false
                do { try persist() }
                catch {
                    busy = true
                    status.text = "Outcome storage failed; original command retained. Reopen and reconcile \(command.id). \(error.localizedDescription)"
                }
            }
            guard let index = records.firstIndex(where: { $0.id == command.id }) else { return }
            do {
                let raw = try await action()
                let value = try Self.json(raw)
                let state = value["status"] as? String ?? "unknown"
                records[index].status = ["confirmed", "rejected", "unknown"].contains(state) ? state : "unknown"
                records[index].result = raw
                status.text = "\(records[index].status.capitalized): \(command.id)\n\(value["error"] as? String ?? "")"
                if let result = value["result"] as? [String: Any], let env = result["environment"] as? [String: Any], let source = env["source"] as? [String: Any] {
                    sandbox.text = source["sandbox_id"] as? String ?? sandbox.text
                    session.text = (env["scope"] as? [String: Any])?["sessionId"] as? String
                    sandboxButton.setTitle("Selected: \(env["name"] as? String ?? sandbox.text ?? "Sandbox") · \(source["lifecycle"] as? String ?? "unknown")", for: .normal)
                    status.text = (status.text ?? "") + "\nObserved lifecycle: \(source["lifecycle"] as? String ?? "unknown")"
                }
            } catch {
                records[index].status = "unknown"
                status.text = "Outcome unknown: \(command.id). Reconcile; do not repeat.\n\(error.localizedDescription)"
            }
        }
    }
    private func persist() throws {
        try FileManager.default.createDirectory(at: storageURL.deletingLastPathComponent(), withIntermediateDirectories: true)
        try JSONEncoder().encode(records).write(to: storageURL, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
    }
    private func browse() {
        guard let client = app.client, let id = session.text, UUID(uuidString: id) != nil, let deployment = Endpoints.deploymentId else { return }
        var url = URLComponents()
        url.scheme = Endpoints.callbackScheme; url.host = "session"
        url.path = "/\(client.orgId())/\(id.lowercased())"
        url.queryItems = [URLQueryItem(name: "deploymentId", value: deployment)]
        if let url = url.url { NotificationCenter.default.post(name: .crewOpenSessionLink, object: url.absoluteString) }
    }
    private static func json(_ raw: String) throws -> [String: Any] {
        guard let value = try JSONSerialization.jsonObject(with: Data(raw.utf8)) as? [String: Any] else { throw CoreError.HostError(message: "Invalid controller response") }
        return value
    }
}
