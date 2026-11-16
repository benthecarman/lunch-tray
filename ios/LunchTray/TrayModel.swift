// The link to the Worker, as in the desktop app's `remote.rs`. The Worker
// owns the tasks and runs the sync; this sends it the changes made here and
// brings its changes back.
//
// A change shows at once: `run` applies the op to `store` and queues it.
// `base` is the Worker's list as of revision `rev`; after each change from
// the Worker, `store` is `base` again with the unconfirmed ops on top, so
// nothing flickers back. While the app is in front, the Worker pushes
// changes down a WebSocket; without one, the app polls.

import Foundation
import Observation
import SwiftUI

struct Notice: Identifiable, Equatable {
    let id = UUID()
    var error: Bool
    var text: String
}

enum ThemePref: String, CaseIterable, Identifiable {
    case system, light, dark
    var id: String { rawValue }
    var label: String { rawValue.capitalized }
    var scheme: ColorScheme? {
        switch self {
        case .system: nil
        case .light: .light
        case .dark: .dark
        }
    }
}

@MainActor
@Observable
final class TrayModel {
    /// What the app shows: the Worker's list with local changes on top.
    private(set) var store = Store()
    private(set) var sync = SyncStatus()
    /// Why the Worker cannot be reached, while it cannot.
    private(set) var offline: String?
    private(set) var notices: [Notice] = []
    private(set) var connected = false
    /// True until the first answer from the Worker or the cache.
    private(set) var loading = true
    private(set) var config: WorkerConfig

    var theme: ThemePref {
        didSet { UserDefaults.standard.set(theme.rawValue, forKey: "theme") }
    }

    // Link state.
    private var base = Store()
    private var rev: UInt64 = 0
    private var workerSync = SyncStatus()
    private var inFlight: OpBatch?
    private var pending: [Op] = []
    private var noticeSeq: UInt64 = 0

    private var runner: Task<Void, Never>?
    private var socket: URLSessionWebSocketTask?
    private var sending = false
    /// Wakes the runner early, say after an op while there is no socket.
    private var wake: CheckedContinuation<Void, Never>?

    private static let pollEvery: Duration = .seconds(10)
    private static let pingEvery: Duration = .seconds(30)
    private static let silenceLimit: TimeInterval = 75
    private static let minBackoff: Double = 5
    private static let maxBackoff: Double = 300

    init() {
        let url = UserDefaults.standard.string(forKey: "workerURL") ?? ""
        config = WorkerConfig(url: url, token: Keychain.read("apiToken") ?? "")
        theme = ThemePref(rawValue: UserDefaults.standard.string(forKey: "theme") ?? "") ?? .system
        loadCache()
    }

    var isConfigured: Bool { config.isComplete }

    // MARK: Changes from the UI

    /// Apply an op here at once and queue it for the Worker. A refusal shows
    /// as a notice and is not sent.
    @discardableResult
    func run(_ op: Op) -> Bool {
        do {
            guard try store.apply(op) else { return true }
        } catch {
            push(Notice(error: true, text: error.localizedDescription))
            return false
        }
        pending.append(op)
        saveCache()
        Task { await sendPending() }
        return true
    }

    func syncNow() {
        guard let client = try? WorkerClient(config: config) else { return }
        sync.inProgress = true
        Task {
            do {
                try await client.syncNow()
            } catch {
                wentOffline(error)
            }
        }
    }

    /// Check for changes now, as on pull to refresh.
    func refresh() async {
        await sendPending()
        guard socket == nil, let client = try? WorkerClient(config: config) else { return }
        do {
            try await fetch(client)
            cameOnline()
        } catch {
            wentOffline(error)
        }
    }

    func dismiss(_ notice: Notice) {
        notices.removeAll { $0.id == notice.id }
    }

    func setConfig(_ new: WorkerConfig) {
        let changedWorker = new.baseURL != config.baseURL
        UserDefaults.standard.set(new.url, forKey: "workerURL")
        Keychain.write("apiToken", new.token)
        config = new
        if changedWorker {
            // Another Worker's tasks are not this one's.
            base = Store()
            store = Store()
            rev = 0
            noticeSeq = 0
            inFlight = nil
            pending = []
            sync = SyncStatus()
            saveCache()
        }
        offline = nil
        stop()
        start()
    }

    // MARK: Lifecycle

    /// Start talking to the Worker, as when the app comes to the front.
    func start() {
        guard runner == nil, isConfigured else {
            if !isConfigured { loading = false }
            return
        }
        runner = Task { await runLoop() }
    }

    /// Close the socket, as when the app goes to the background.
    func stop() {
        runner?.cancel()
        runner = nil
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
        connected = false
        poke()
    }

    private func poke() {
        wake?.resume()
        wake = nil
    }

    /// Sleep for `d`, or until poked or cancelled.
    private func nap(_ d: Duration) async {
        let timer = Task { [weak self] in
            try? await Task.sleep(for: d)
            self?.poke()
        }
        await withTaskCancellationHandler {
            await withCheckedContinuation { cont in
                if Task.isCancelled {
                    cont.resume()
                } else {
                    poke()
                    wake = cont
                }
            }
        } onCancel: {
            Task { @MainActor [weak self] in self?.poke() }
        }
        timer.cancel()
    }

    private func runLoop() async {
        var backoff = Self.minBackoff
        var retryAt = Date.distantPast
        while !Task.isCancelled {
            guard let client = try? WorkerClient(config: config) else { return }
            await sendPending()
            do {
                try await fetch(client)
                cameOnline()
            } catch {
                if Task.isCancelled { return }
                wentOffline(error)
                await nap(Self.pollEvery)
                continue
            }
            if rev != 0 && Date.now >= retryAt {
                let opened = Date.now
                do {
                    try await socketSession(client)
                } catch {}
                if Task.isCancelled { return }
                // A socket that keeps dropping right away waits longer.
                if Date.now.timeIntervalSince(opened) > 60 { backoff = Self.minBackoff }
                retryAt = Date.now.addingTimeInterval(backoff)
                backoff = min(backoff * 2, Self.maxBackoff)
                // Catch up over HTTP right away.
                continue
            }
            await nap(Self.pollEvery)
        }
    }

    /// Runs until the socket fails or the runner is cancelled.
    private func socketSession(_ client: WorkerClient) async throws {
        let ws = client.socket()
        socket = ws
        ws.resume()
        try await ws.send(.string(hello()))
        connected = true
        let heard = Heard()
        let watchdog = Task { [weak ws] in
            while !Task.isCancelled {
                try? await Task.sleep(for: Self.pingEvery)
                guard let ws, !Task.isCancelled else { return }
                if Date.now.timeIntervalSince(heard.at) > Self.silenceLimit {
                    ws.cancel(with: .abnormalClosure, reason: nil)
                    return
                }
                try? await ws.send(.string("ping"))
            }
        }
        defer {
            watchdog.cancel()
            ws.cancel(with: .goingAway, reason: nil)
            // A runner started since, say for another Worker, has its own.
            if socket === ws {
                socket = nil
                connected = false
            }
        }
        while !Task.isCancelled {
            let msg = try await ws.receive()
            heard.at = Date.now
            let text: String
            switch msg {
            case .string(let s): text = s
            case .data(let d): text = String(decoding: d, as: UTF8.self)
            @unknown default: continue
            }
            if text == "pong" { continue }
            let push = try JSON.decoder.decode(Push.self, from: Data(text.utf8))
            switch push {
            case .delta(let delta):
                take(delta)
            case .resync:
                if let full = try await client.changes(since: 0) { take(full) }
                try await ws.send(.string(hello()))
            }
        }
    }

    /// When the Worker last sent anything, pongs included.
    private final class Heard {
        var at = Date.now
    }

    private func hello() -> String { "{\"since\":\(rev)}" }

    // MARK: The link

    private func fetch(_ client: WorkerClient) async throws {
        if let delta = try await client.changes(since: rev) { take(delta) }
        loading = false
    }

    /// Send the queued ops. Over a socket the Worker pushes the result;
    /// without one, fetch it.
    private func sendPending() async {
        guard !sending, let client = try? WorkerClient(config: config) else { return }
        sending = true
        defer { sending = false }
        while inFlight != nil || !pending.isEmpty {
            if inFlight == nil {
                inFlight = OpBatch(id: UUID().uuidString.lowercased(), ops: pending)
                pending = []
            }
            do {
                let results = try await client.ops(inFlight!)
                inFlight = nil
                for r in results {
                    // The Worker refused: its list wins with the next changes.
                    if let e = r.error { push(Notice(error: true, text: e)) }
                }
                cameOnline()
            } catch {
                // Resent later with the same id, which the Worker recognizes.
                wentOffline(error)
                saveCache()
                return
            }
        }
        saveCache()
        if socket == nil {
            try? await fetch(client)
        }
    }

    private func take(_ delta: Delta) {
        let first = rev == 0
        rev = delta.rev
        workerSync = delta.sync
        base.apply(delta: delta)
        var view = base
        let unconfirmed = (inFlight?.ops ?? []) + pending
        for op in unconfirmed { _ = try? view.apply(op) }
        if view != store { store = view }
        if offline == nil { sync = delta.sync }
        for n in delta.notices where n.seq > noticeSeq {
            noticeSeq = n.seq
            if !first { push(Notice(error: n.error, text: n.text)) }
        }
        loading = false
        saveCache()
    }

    private func wentOffline(_ error: Error) {
        if Task.isCancelled || (error as? URLError)?.code == .cancelled { return }
        let why = "Worker unreachable: \(error.localizedDescription)"
        if offline == nil, !(error is CancellationError) {
            push(Notice(error: true, text: why))
        }
        offline = why
        sync.errors = [why]
        sync.inProgress = false
        loading = false
    }

    private func cameOnline() {
        guard offline != nil else { return }
        offline = nil
        sync = workerSync
    }

    private func push(_ n: Notice) {
        notices.append(n)
        if notices.count > 3 { notices.removeFirst(notices.count - 3) }
        Task {
            try? await Task.sleep(for: .seconds(n.error ? 8 : 5))
            dismiss(n)
        }
    }

    // MARK: Cache

    private struct Cache: Codable {
        var base: Store
        var rev: UInt64
        var noticeSeq: UInt64
        var sync: SyncStatus
        var inFlight: [Op]?
        var inFlightId: String?
        var pending: [Op]
        var worker: String
    }

    private static var cacheURL: URL {
        URL.applicationSupportDirectory.appending(path: "remote-cache.json")
    }

    private func loadCache() {
        guard let data = try? Data(contentsOf: Self.cacheURL),
            let c = try? JSON.decoder.decode(Cache.self, from: data),
            c.worker == config.baseURL?.absoluteString
        else { return }
        base = c.base
        rev = c.rev
        noticeSeq = c.noticeSeq
        workerSync = c.sync
        sync = c.sync
        pending = c.pending
        if let ops = c.inFlight, let id = c.inFlightId { inFlight = OpBatch(id: id, ops: ops) }
        var view = base
        for op in (inFlight?.ops ?? []) + pending { _ = try? view.apply(op) }
        store = view
        loading = false
    }

    private func saveCache() {
        let c = Cache(
            base: base, rev: rev, noticeSeq: noticeSeq, sync: workerSync,
            inFlight: inFlight?.ops, inFlightId: inFlight?.id, pending: pending,
            worker: config.baseURL?.absoluteString ?? "")
        do {
            try FileManager.default.createDirectory(
                at: URL.applicationSupportDirectory, withIntermediateDirectories: true)
            try JSON.encoder.encode(c).write(to: Self.cacheURL, options: .atomic)
        } catch {
            print("save cache: \(error)")
        }
    }
}
