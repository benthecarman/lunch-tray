import SwiftUI

enum Editing: Identifiable {
    case new
    case task(String)

    var id: String {
        switch self {
        case .new: "new"
        case .task(let id): id
        }
    }
}

struct ContentView: View {
    @Environment(TrayModel.self) private var model
    @Environment(\.openURL) private var openURL

    @State private var query = ""
    @State private var editing: Editing?
    @State private var showSettings = false
    @State private var deleting: TrayTask?
    @State private var confirmArchiveOld = false
    @AppStorage("collapsed.pinned") private var pinnedCollapsed = false
    @AppStorage("collapsed.active") private var activeCollapsed = false
    @AppStorage("collapsed.settled") private var settledCollapsed = false
    @AppStorage("collapsed.archived") private var archivedCollapsed = true

    var body: some View {
        NavigationStack {
            content
                .background(Palette.bg.ignoresSafeArea())
                .navigationTitle("Lunch Tray")
                .navigationBarTitleDisplayMode(.inline)
                .toolbarBackground(Palette.bg, for: .navigationBar)
                .toolbar { toolbar }
                .searchable(text: $query, prompt: "Org, repo, title, or number")
                .safeAreaInset(edge: .bottom, spacing: 0) { footer }
                .overlay(alignment: .bottom) { NoticeStack().padding(.bottom, 44) }
        }
        .sheet(item: $editing) { e in
            TaskEditor(editing: e).environment(model)
        }
        .sheet(isPresented: $showSettings) {
            SettingsView().environment(model)
        }
        .onAppear {
            if !model.isConfigured { showSettings = true }
        }
        .confirmationDialog(
            "Delete \u{201C}\(deleting?.title ?? "")\u{201D}?", isPresented: deleteShown,
            titleVisibility: .visible, presenting: deleting
        ) { t in
            Button("Delete", role: .destructive) { model.run(.delete(id: t.id)) }
        } message: { _ in
            Text("This cannot be undone.")
        }
        .confirmationDialog(
            "Archive \(staleIds.count) tasks with no activity in a year?",
            isPresented: $confirmArchiveOld, titleVisibility: .visible
        ) {
            Button("Archive \(staleIds.count)") { model.run(.archiveMany(ids: staleIds)) }
        } message: {
            Text("They come back on new activity like any archived task.")
        }
    }

    private var deleteShown: Binding<Bool> {
        Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } })
    }

    private var staleIds: [String] {
        model.store.staleIds(before: Date.now.addingTimeInterval(-365 * 86_400))
    }

    @ViewBuilder private var content: some View {
        let s = model.store.sections(matching: query)
        let searching = !query.trimmingCharacters(in: .whitespaces).isEmpty
        if !model.isConfigured {
            ContentUnavailableView {
                Label("No Worker yet", systemImage: "tray")
            } description: {
                Text("Point Lunch Tray at your Worker to see your tasks.")
            } actions: {
                Button("Set up") { showSettings = true }.buttonStyle(.borderedProminent)
            }
        } else if model.loading && model.store.tasks.isEmpty {
            ProgressView("Loading tasks…")
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if searching && s.pinned.isEmpty && s.active.isEmpty && s.settled.isEmpty
            && s.archived.isEmpty
        {
            ContentUnavailableView.search(text: query)
        } else {
            List {
                section("Pinned", s.pinned, $pinnedCollapsed, searching: searching, hideEmpty: true)
                section("Active", s.active, $activeCollapsed, searching: searching)
                section("Settled", s.settled, $settledCollapsed, searching: searching, hideEmpty: true)
                section(
                    "Archived", s.archived, $archivedCollapsed, searching: searching,
                    hideEmpty: true)
            }
            .listStyle(.insetGrouped)
            .scrollContentBackground(.hidden)
            .refreshable { await model.refresh() }
        }
    }

    @ViewBuilder
    private func section(
        _ name: String, _ tasks: [TrayTask], _ collapsed: Binding<Bool>, searching: Bool,
        hideEmpty: Bool = false
    ) -> some View {
        if !(hideEmpty && tasks.isEmpty) {
            let expanded = searching || !collapsed.wrappedValue
            Section {
                if expanded {
                    if tasks.isEmpty {
                        Text(name == "Active" ? "Nothing needs you right now." : "None")
                            .font(.subheadline)
                            .foregroundStyle(Palette.textWeak)
                            .listRowBackground(Palette.well)
                    }
                    ForEach(tasks) { t in
                        TaskRow(item: t)
                            .contentShape(Rectangle())
                            .onTapGesture { open(t) }
                            .listRowBackground(
                                t.pinned ? Palette.accent.opacity(0.14) : Palette.well
                            )
                            .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                                trailingSwipe(t)
                            }
                            .swipeActions(edge: .leading, allowsFullSwipe: true) {
                                leadingSwipe(t)
                            }
                            .contextMenu { menu(t) }
                    }
                }
            } header: {
                Button {
                    withAnimation { collapsed.wrappedValue.toggle() }
                } label: {
                    HStack(spacing: 6) {
                        Image(systemName: "chevron.right")
                            .font(.caption2.weight(.bold))
                            .rotationEffect(.degrees(expanded ? 90 : 0))
                        Text(name)
                        Text("\(tasks.count)").foregroundStyle(Palette.textWeak)
                        Spacer()
                    }
                    .font(.subheadline.weight(.medium))
                    .foregroundStyle(name == "Pinned" ? Palette.accent : Palette.textWeak)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .disabled(searching)
                .textCase(nil)
            }
        }
    }

    // MARK: Actions

    private func open(_ t: TrayTask) {
        model.run(.markSeen(id: t.id))
        if let url = t.url {
            openURL(url)
        } else if t.isManual {
            editing = .task(t.id)
        }
    }

    @ViewBuilder private func trailingSwipe(_ t: TrayTask) -> some View {
        switch t.rung {
        case .active:
            Button { model.run(.settle(id: t.id)) } label: {
                Label("Settle", systemImage: "checkmark")
            }
            .tint(Palette.open)
            Button { model.run(.archive(id: t.id)) } label: {
                Label("Archive", systemImage: "archivebox")
            }
            .tint(Palette.textWeak)
        case .settled:
            Button { model.run(.archive(id: t.id)) } label: {
                Label("Archive", systemImage: "archivebox")
            }
            .tint(Palette.textWeak)
        case .archived:
            if !t.isRunning {
                Button(role: .destructive) { deleting = t } label: {
                    Label("Delete", systemImage: "trash")
                }
            }
        }
    }

    @ViewBuilder private func leadingSwipe(_ t: TrayTask) -> some View {
        switch t.rung {
        case .active:
            Button { model.run(.setPinned(id: t.id, pinned: !t.pinned)) } label: {
                Label(t.pinned ? "Unpin" : "Pin", systemImage: t.pinned ? "pin.slash" : "pin")
            }
            .tint(Palette.accent)
        case .settled, .archived:
            Button { model.run(.reopen(id: t.id)) } label: {
                Label("Reopen", systemImage: "arrow.uturn.backward")
            }
            .tint(Palette.open)
        }
    }

    @ViewBuilder private func menu(_ t: TrayTask) -> some View {
        if let url = t.url {
            Button { open(t) } label: { Label("Open", systemImage: "safari") }
            ShareLink(item: url) { Label("Share Link", systemImage: "square.and.arrow.up") }
        }
        Button { editing = .task(t.id) } label: {
            Label(t.isManual ? "Edit" : "Rename", systemImage: "pencil")
        }
        if t.unseen {
            Button { model.run(.markSeen(id: t.id)) } label: {
                Label("Mark as Read", systemImage: "circle.slash")
            }
        }
        Divider()
        switch t.rung {
        case .active:
            Button { model.run(.setPinned(id: t.id, pinned: !t.pinned)) } label: {
                Label(t.pinned ? "Unpin" : "Pin", systemImage: t.pinned ? "pin.slash" : "pin")
            }
            Button { model.run(.settle(id: t.id)) } label: {
                Label("Settle", systemImage: "checkmark")
            }
            Button { model.run(.archive(id: t.id)) } label: {
                Label("Archive", systemImage: "archivebox")
            }
        case .settled:
            Button { model.run(.reopen(id: t.id)) } label: {
                Label("Reopen", systemImage: "arrow.uturn.backward")
            }
            Button { model.run(.archive(id: t.id)) } label: {
                Label("Archive", systemImage: "archivebox")
            }
        case .archived:
            Button { model.run(.reopen(id: t.id)) } label: {
                Label("Reopen", systemImage: "arrow.uturn.backward")
            }
        }
        Button(role: .destructive) { deleting = t } label: {
            Label("Delete", systemImage: "trash")
        }
        .disabled(t.isRunning)
    }

    // MARK: Chrome

    @ToolbarContentBuilder private var toolbar: some ToolbarContent {
        ToolbarItem(placement: .topBarLeading) {
            Menu {
                Button { model.syncNow() } label: {
                    Label("Sync Now", systemImage: "arrow.triangle.2.circlepath")
                }
                Button { model.run(.markAllSeen) } label: {
                    Label("Mark All as Read", systemImage: "checkmark.circle")
                }
                .disabled(model.store.tasks.allSatisfy { !$0.unseen })
                Button { confirmArchiveOld = true } label: {
                    Label("Archive Older Than a Year", systemImage: "archivebox")
                }
                .disabled(staleIds.isEmpty)
                Divider()
                Button { showSettings = true } label: {
                    Label("Settings", systemImage: "gearshape")
                }
            } label: {
                Image(systemName: "ellipsis.circle")
            }
        }
        ToolbarItem(placement: .principal) {
            HStack(spacing: 8) {
                TrayMark()
                    .stroke(Palette.text, lineWidth: 2)
                    .frame(width: 20, height: 20)
                Text("Lunch Tray").font(.headline)
            }
        }
        ToolbarItem(placement: .topBarTrailing) {
            Button { editing = .new } label: { Image(systemName: "plus") }
                .disabled(!model.isConfigured)
        }
    }

    private var footer: some View {
        HStack(spacing: 6) {
            let sync = model.sync
            Group {
                if sync.inProgress {
                    ProgressView().controlSize(.mini)
                    Text("Syncing…")
                } else if !sync.errors.isEmpty {
                    Image(systemName: "exclamationmark.circle").foregroundStyle(Palette.warning)
                    Text(model.offline != nil ? "Offline" : "Sync error")
                        .foregroundStyle(Palette.warning)
                } else if let t = sync.lastFinished {
                    Image(systemName: model.connected ? "bolt.horizontal.circle" : "checkmark.circle")
                    TimelineView(.periodic(from: .now, by: 30)) { ctx in
                        Text("Synced \(ago(t, now: ctx.date))")
                    }
                } else {
                    Image(systemName: "circle.dashed")
                    Text("Not synced yet")
                }
            }
            Spacer()
            let s = model.store.sections()
            Text(
                s.archived.isEmpty
                    ? "\(s.settled.count) settled"
                    : "\(s.settled.count) settled, \(s.archived.count) archived")
        }
        .font(.caption)
        .foregroundStyle(Palette.textWeak)
        .padding(.horizontal, 20)
        .padding(.vertical, 10)
        .background(Palette.bg)
        .overlay(alignment: .top) { Divider() }
    }
}

struct NoticeStack: View {
    @Environment(TrayModel.self) private var model

    var body: some View {
        VStack(spacing: 8) {
            ForEach(model.notices) { n in
                HStack(alignment: .top, spacing: 10) {
                    Image(systemName: n.error ? "exclamationmark.triangle.fill" : "info.circle.fill")
                        .foregroundStyle(n.error ? Palette.danger : Palette.accent)
                    Text(n.text).font(.subheadline).foregroundStyle(Palette.text)
                    Spacer(minLength: 0)
                }
                .padding(12)
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
                .onTapGesture { withAnimation { model.dismiss(n) } }
                .transition(.move(edge: .bottom).combined(with: .opacity))
            }
        }
        .padding(.horizontal, 16)
        .animation(.default, value: model.notices)
    }
}

func ago(_ t: Date, now: Date = .now) -> String {
    let day = 86_400
    let secs = max(0, Int(now.timeIntervalSince(t)))
    return switch secs {
    case ..<60: "just now"
    case ..<3600: "\(secs / 60)m ago"
    case ..<day: "\(secs / 3600)h ago"
    case ..<(30 * day): "\(secs / day)d ago"
    case ..<(365 * day): "\(secs / (30 * day))mo ago"
    default: "\(secs / (365 * day))y ago"
    }
}
