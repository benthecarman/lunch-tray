import SwiftUI

/// Add a task, or rename one and edit a manual task's notes and link.
struct TaskEditor: View {
    let editing: Editing

    @Environment(TrayModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var title = ""
    @State private var notes = ""
    @State private var link = ""
    @State private var loaded = false
    @FocusState private var titleFocused: Bool

    private var existing: TrayTask? {
        if case .task(let id) = editing { return model.store.task(id) }
        return nil
    }

    private var manual: Bool { existing?.isManual ?? true }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Title", text: $title, axis: .vertical)
                        .focused($titleFocused)
                        .submitLabel(.done)
                }
                if manual {
                    Section("Notes") {
                        TextField("Notes", text: $notes, axis: .vertical)
                            .lineLimit(3...10)
                    }
                    Section("Link") {
                        TextField("https://", text: $link)
                            .keyboardType(.URL)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                    }
                } else if let r = existing?.remote {
                    Section {
                        LabeledContent("Item", value: r.label)
                        if let original = existing?.remoteTitle, original != title {
                            LabeledContent("Title on GitHub", value: original)
                        }
                    } footer: {
                        Text("A rename stays until the title changes on GitHub.")
                    }
                }
            }
            .scrollContentBackground(.hidden)
            .background(Palette.bg)
            .navigationTitle(existing == nil ? "New Task" : (manual ? "Edit Task" : "Rename"))
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button(existing == nil ? "Add" : "Save") { save() }
                        .disabled(title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                }
            }
            .onAppear(perform: load)
        }
        .presentationDetents(manual ? [.large] : [.medium])
    }

    private func load() {
        guard !loaded else { return }
        loaded = true
        if let t = existing {
            title = t.title
            notes = t.notes
            link = t.link ?? ""
        } else {
            titleFocused = true
        }
    }

    private func save() {
        guard let t = existing else {
            if model.run(.add(title: title, notes: notes, link: link)) { dismiss() }
            return
        }
        guard model.run(.rename(id: t.id, title: title)) else { return }
        if manual {
            model.run(.editManual(id: t.id, notes: notes, link: link))
        }
        dismiss()
    }
}
