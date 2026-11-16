import SwiftUI

struct SettingsView: View {
    @Environment(TrayModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var url = ""
    @State private var token = ""
    @State private var checking = false
    @State private var result: Result<Int, Error>?

    private var draft: WorkerConfig { WorkerConfig(url: url, token: token) }

    var body: some View {
        @Bindable var model = model
        NavigationStack {
            Form {
                Section {
                    TextField("https://lunch-tray.example.workers.dev", text: $url)
                        .keyboardType(.URL)
                        .textContentType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    SecureField("API token", text: $token)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                } header: {
                    Text("Worker")
                } footer: {
                    Text(
                        "The Worker's URL and its API_TOKEN, as in the desktop app's config.toml. The token is kept in the keychain."
                    )
                }
                Section {
                    Button {
                        Task { await check() }
                    } label: {
                        HStack {
                            Text("Test Connection")
                            Spacer()
                            if checking { ProgressView() }
                        }
                    }
                    .disabled(!draft.isComplete || checking)
                    switch result {
                    case .success(let n):
                        Label("Connected: \(n) tasks", systemImage: "checkmark.circle")
                            .foregroundStyle(Palette.open)
                    case .failure(let e):
                        Label(e.localizedDescription, systemImage: "exclamationmark.triangle")
                            .foregroundStyle(Palette.danger)
                    case nil:
                        EmptyView()
                    }
                }
                Section("Appearance") {
                    Picker("Theme", selection: $model.theme) {
                        ForEach(ThemePref.allCases) { Text($0.label).tag($0) }
                    }
                }
            }
            .scrollContentBackground(.hidden)
            .background(Palette.bg)
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        if draft != model.config { model.setConfig(draft) }
                        dismiss()
                    }
                    .disabled(!draft.isComplete)
                }
            }
            .onAppear {
                url = model.config.url
                token = model.config.token
            }
            .onChange(of: draft) { result = nil }
        }
    }

    private func check() async {
        checking = true
        defer { checking = false }
        do {
            let client = try WorkerClient(config: draft)
            let full = try await client.changes(since: 0)
            result = .success(full?.tasks.count ?? 0)
        } catch {
            result = .failure(error)
        }
    }
}
