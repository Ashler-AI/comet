import SwiftUI

struct NotificationSettingsView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    private var notifications: SessionNotifications { model.notifications }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Toggle("Session attention alerts", isOn: Binding(
                        get: { notifications.enabled },
                        set: { value in Task { await notifications.setEnabled(value) } }
                    ))
                    .disabled(notifications.busy || model.demo != nil)
                    if notifications.busy { ProgressView() }
                    Text(model.demo != nil ? "Sign in to enable notifications." : notifications.status)
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                    if let error = notifications.error {
                        Text(error).font(.footnote).foregroundStyle(Theme.danger)
                    }
                    if notifications.enabled {
                        Button("Retry background registration") { notifications.refresh() }
                            .disabled(notifications.busy)
                    }
                    Button("Open iOS notification settings") {
                        if let url = URL(string: UIApplication.openNotificationSettingsURLString) {
                            UIApplication.shared.open(url)
                        }
                    }
                } footer: {
                    Text("Crew alerts you when a session needs input, encounters an error, or finishes. Session names may appear on your lock screen; transcript content is not included. Background delivery stores your push token privately and your revocable sign-in credential encrypted on the Crew server to recheck access before each alert. Sign-out always completes locally and attempts to remove registration in the background. Disabling alerts requires server confirmation. Background delivery requires the server's Apple push configuration.")
                }
            }
            .navigationTitle("Notifications")
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
        }
    }
}
