import UIKit
import UniformTypeIdentifiers

/// An explicitly selected copied backup is quarantined in this verified profile.
/// No legacy snapshot is ever connected to the live upstream sync transport.
final class CrewMigrationViewController: UIViewController, UIDocumentPickerDelegate {
    private let app: AppModel
    private let status = UITextView()
    private var lastDirectory: URL?
    private let storageKey: String

    init(app: AppModel) {
        self.app = app
        self.storageKey = "crew.legacyImport.\(app.crewStorageKey ?? "signed-out")"
        super.init(nibName: nil, bundle: nil)
        title = "Legacy iOS backup"
    }
    required init?(coder: NSCoder) { fatalError() }
    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = Palette.background
        status.isEditable = false
        status.font = UIFont.preferredFont(forTextStyle: .body)
        status.text = "Select a consistent COPY of the old app's Application Support/CometDocs folder. Keep the old app installed and do not clear its data.\n\nCrew Canary preserves every selected file with a SHA-256 manifest, exports readable snapshots as offline JSON, and quarantines pending commands/outcomes without executing them. No old credentials are imported.\n\nThis is preservation, not permission to publish cached records. Reconcile retained session IDs and pending outcomes through the authenticated desktop importer before cutover. Cache-only records remain available in the exported archive; do not uninstall the old app until reconciliation is complete."
        status.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(status)
        NSLayoutConstraint.activate([
            status.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor, constant: 16),
            status.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor, constant: -16),
            status.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor),
            status.bottomAnchor.constraint(equalTo: view.safeAreaLayoutGuide.bottomAnchor),
        ])
        navigationItem.rightBarButtonItems = [
            UIBarButtonItem(title: "Import copy", primaryAction: UIAction { [weak self] _ in self?.pickBackup() }),
            UIBarButtonItem(title: "Export", primaryAction: UIAction { [weak self] _ in self?.exportBackup() }),
        ]
        if let path = UserDefaults.standard.string(forKey: storageKey) {
            lastDirectory = URL(fileURLWithPath: path, isDirectory: true)
            if let data = try? Data(contentsOf: lastDirectory!.appendingPathComponent("manifest.json")), let text = String(data: data, encoding: .utf8) {
                status.text += "\n\nLast preservation report:\n\(text)"
            }
        }
    }
    private func pickBackup() {
        let picker = UIDocumentPickerViewController(forOpeningContentTypes: [.folder], asCopy: false)
        picker.delegate = self
        picker.allowsMultipleSelection = false
        present(picker, animated: true)
    }
    func documentPicker(_ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]) {
        guard let source = urls.first, let client = app.client else { return }
        let access = source.startAccessingSecurityScopedResource()
        navigationItem.rightBarButtonItems?.forEach { $0.isEnabled = false }
        Task { @MainActor in
            defer {
                if access { source.stopAccessingSecurityScopedResource() }
                navigationItem.rightBarButtonItems?.forEach { $0.isEnabled = true }
            }
            do {
                let report = try await client.crewImportLegacyBackup(sourcePath: source.path)
                guard let value = try JSONSerialization.jsonObject(with: Data(report.utf8)) as? [String: Any],
                      let path = value["directory"] as? String else { throw CoreError.Storage(message: "Missing migration report") }
                lastDirectory = URL(fileURLWithPath: path, isDirectory: true)
                UserDefaults.standard.set(path, forKey: storageKey)
                let data = try JSONSerialization.data(withJSONObject: value, options: [.prettyPrinted, .sortedKeys])
                status.text = "Original backup untouched. Preserved files are isolated from live sync. Pending work was NOT replayed. Export this archive for authenticated reconciliation.\n\n" + String(decoding: data, as: UTF8.self)
            } catch { status.text += "\n\nImport stopped; originals retained. \(error.localizedDescription)" }
        }
    }
    private func exportBackup() {
        guard let lastDirectory else { return }
        // The native export picker gives the user explicit control over disclosure.
        present(UIDocumentPickerViewController(forExporting: [lastDirectory], asCopy: true), animated: true)
    }
}
