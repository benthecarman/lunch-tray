// The task list and the ops that change it. Mirrors `crates/core/src/ops.rs`
// and the transitions in `model.rs`, so a change shows here at once and the
// Worker applies the same op with the same rules.

import Foundation

enum Op: Codable, Equatable, Sendable {
    case add(title: String, notes: String, link: String?)
    case rename(id: String, title: String)
    case editManual(id: String, notes: String, link: String?)
    case setPinned(id: String, pinned: Bool)
    case settle(id: String)
    case reopen(id: String)
    case archive(id: String)
    case archiveMany(ids: [String])
    case delete(id: String)
    case markSeen(id: String)
    case markAllSeen

    private enum Keys: String, CodingKey { case op, id, ids, title, notes, link, pinned }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: Keys.self)
        switch self {
        case let .add(title, notes, link):
            try c.encode("add", forKey: .op)
            try c.encode(title, forKey: .title)
            try c.encode(notes, forKey: .notes)
            try c.encode(link, forKey: .link)
        case let .rename(id, title):
            try c.encode("rename", forKey: .op)
            try c.encode(id, forKey: .id)
            try c.encode(title, forKey: .title)
        case let .editManual(id, notes, link):
            try c.encode("edit_manual", forKey: .op)
            try c.encode(id, forKey: .id)
            try c.encode(notes, forKey: .notes)
            try c.encode(link, forKey: .link)
        case let .setPinned(id, pinned):
            try c.encode("set_pinned", forKey: .op)
            try c.encode(id, forKey: .id)
            try c.encode(pinned, forKey: .pinned)
        case let .settle(id):
            try c.encode("settle", forKey: .op)
            try c.encode(id, forKey: .id)
        case let .reopen(id):
            try c.encode("reopen", forKey: .op)
            try c.encode(id, forKey: .id)
        case let .archive(id):
            try c.encode("archive", forKey: .op)
            try c.encode(id, forKey: .id)
        case let .archiveMany(ids):
            try c.encode("archive_many", forKey: .op)
            try c.encode(ids, forKey: .ids)
        case let .delete(id):
            try c.encode("delete", forKey: .op)
            try c.encode(id, forKey: .id)
        case let .markSeen(id):
            try c.encode("mark_seen", forKey: .op)
            try c.encode(id, forKey: .id)
        case .markAllSeen:
            try c.encode("mark_all_seen", forKey: .op)
        }
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        let id = { try c.decode(String.self, forKey: .id) }
        switch try c.decode(String.self, forKey: .op) {
        case "add":
            self = .add(
                title: try c.decode(String.self, forKey: .title),
                notes: try c.decodeIfPresent(String.self, forKey: .notes) ?? "",
                link: try c.decodeIfPresent(String.self, forKey: .link))
        case "rename": self = .rename(id: try id(), title: try c.decode(String.self, forKey: .title))
        case "edit_manual":
            self = .editManual(
                id: try id(), notes: try c.decode(String.self, forKey: .notes),
                link: try c.decodeIfPresent(String.self, forKey: .link))
        case "set_pinned":
            self = .setPinned(id: try id(), pinned: try c.decode(Bool.self, forKey: .pinned))
        case "settle": self = .settle(id: try id())
        case "reopen": self = .reopen(id: try id())
        case "archive": self = .archive(id: try id())
        case "archive_many": self = .archiveMany(ids: try c.decode([String].self, forKey: .ids))
        case "delete": self = .delete(id: try id())
        case "mark_seen": self = .markSeen(id: try id())
        case "mark_all_seen": self = .markAllSeen
        case let other:
            throw DecodingError.dataCorruptedError(
                forKey: .op, in: c, debugDescription: "unknown op \(other)")
        }
    }
}

struct OpBatch: Encodable, Sendable {
    var id: String
    var ops: [Op]
}

/// One answer per op: serde's `Result`, `{"Ok":{...}}` or `{"Err":"..."}`.
struct OpResult: Decodable, Sendable {
    struct Applied: Decodable, Sendable {
        var changed: Bool
        var id: String?
    }

    var applied: Applied?
    var error: String?

    private enum Keys: String, CodingKey { case Ok, Err }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        applied = try c.decodeIfPresent(Applied.self, forKey: .Ok)
        error = try c.decodeIfPresent(String.self, forKey: .Err)
    }
}

struct OpError: Error, LocalizedError {
    var message: String
    var errorDescription: String? { message }
}

/// The tasks visible in each section, newest activity first.
struct Sections {
    var pinned: [TrayTask] = []
    var active: [TrayTask] = []
    var settled: [TrayTask] = []
    var archived: [TrayTask] = []
}

struct Store: Codable, Equatable, Sendable {
    var tasks: [TrayTask] = []
    var nextManualId: UInt64 = 0

    enum CodingKeys: String, CodingKey {
        case tasks
        case nextManualId = "next_manual_id"
    }

    func task(_ id: String) -> TrayTask? { tasks.first { $0.id == id } }

    private func index(_ id: String) -> Int? { tasks.firstIndex { $0.id == id } }

    mutating func apply(delta: Delta) {
        if delta.full {
            tasks = delta.tasks
        } else {
            if !delta.removed.isEmpty {
                let removed = Set(delta.removed)
                tasks.removeAll { removed.contains($0.id) }
            }
            var at = Dictionary(uniqueKeysWithValues: tasks.enumerated().map { ($1.id, $0) })
            for t in delta.tasks {
                if let i = at[t.id] {
                    tasks[i] = t
                } else {
                    at[t.id] = tasks.count
                    tasks.append(t)
                }
            }
        }
        nextManualId = delta.nextManualId
    }

    func sections(matching query: String = "") -> Sections {
        var s = Sections()
        let q = query.trimmingCharacters(in: .whitespaces)
        for t in tasks where q.isEmpty || t.matches(q) {
            switch t.rung {
            case .archived: s.archived.append(t)
            case .active where t.pinned: s.pinned.append(t)
            case .active: s.active.append(t)
            case .settled: s.settled.append(t)
            }
        }
        let newest: (TrayTask, TrayTask) -> Bool = { $0.lastActivity > $1.lastActivity }
        s.pinned.sort(by: newest)
        s.active.sort(by: newest)
        s.settled.sort(by: newest)
        s.archived.sort(by: newest)
        return s
    }

    var unseenCount: Int { tasks.filter { $0.unseen && $0.state == .active }.count }

    /// Unarchived, unpinned tasks with no activity since `cutoff`.
    func staleIds(before cutoff: Date) -> [String] {
        tasks.filter { $0.state != .archived && !$0.pinned && $0.lastActivity < cutoff }
            .map(\.id)
    }

    // MARK: Transitions
    // Each returns true when something changed and throws a refusal.

    @discardableResult
    mutating func apply(_ op: Op, now: Date = .now) throws -> Bool {
        switch op {
        case let .add(title, notes, link):
            let title = title.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !title.isEmpty else { throw OpError(message: "A task needs a title.") }
            nextManualId += 1
            tasks.append(
                TrayTask(
                    id: "manual:\(nextManualId)", title: title,
                    notes: notes.trimmingCharacters(in: .whitespacesAndNewlines),
                    link: Self.cleanLink(link), project: "Local", state: .active,
                    createdAt: now, updatedAt: now, origin: .manual))
            return true
        case let .rename(id, title):
            let title = title.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !title.isEmpty else { throw OpError(message: "A task needs a title.") }
            guard let i = index(id), tasks[i].title != title else { return false }
            tasks[i].title = title
            tasks[i].updatedAt = now
            return true
        case let .editManual(id, notes, link):
            guard let i = index(id) else { return false }
            guard tasks[i].isManual else {
                throw OpError(message: "Only manual tasks have editable notes.")
            }
            let notes = notes.trimmingCharacters(in: .whitespacesAndNewlines)
            let link = Self.cleanLink(link)
            guard tasks[i].notes != notes || tasks[i].link != link else { return false }
            tasks[i].notes = notes
            tasks[i].link = link
            tasks[i].updatedAt = now
            return true
        case let .setPinned(id, pinned):
            guard let i = index(id) else { return false }
            guard tasks[i].state == .active else {
                throw OpError(message: "Only active tasks can be pinned.")
            }
            guard tasks[i].pinned != pinned else { return false }
            tasks[i].pinned = pinned
            return true
        case let .settle(id):
            guard let i = index(id), tasks[i].state == .active else { return false }
            tasks[i].state = .settled
            tasks[i].pinned = false
            tasks[i].unseen = false
            tasks[i].updatedAt = now
            return true
        case let .reopen(id):
            guard let i = index(id), tasks[i].state != .active else { return false }
            tasks[i].state = .active
            tasks[i].pinned = false
            tasks[i].updatedAt = now
            // The last check result is due for a refresh.
            if case .remote(var r) = tasks[i].origin, r.checks != nil {
                r.checks?.checkedAt = .distantPast
                tasks[i].origin = .remote(r)
            }
            return true
        case let .archive(id):
            guard let i = index(id), tasks[i].state != .archived else { return false }
            tasks[i].state = .archived
            tasks[i].updatedAt = now
            return true
        case let .archiveMany(ids):
            var any = false
            for id in ids where (try? apply(.archive(id: id), now: now)) == true { any = true }
            return any
        case let .delete(id):
            guard let i = index(id) else { return false }
            if let r = tasks[i].remote, r.state == .open {
                throw OpError(
                    message:
                        "Cannot delete \(r.label): the \(r.kind == .pullRequest ? "pull request" : "issue") is still open. Settle it instead."
                )
            }
            tasks.remove(at: i)
            return true
        case let .markSeen(id):
            guard let i = index(id), tasks[i].unseen else { return false }
            tasks[i].unseen = false
            return true
        case .markAllSeen:
            var any = false
            for i in tasks.indices where tasks[i].unseen {
                tasks[i].unseen = false
                any = true
            }
            return any
        }
    }

    private static func cleanLink(_ link: String?) -> String? {
        guard let l = link?.trimmingCharacters(in: .whitespacesAndNewlines), !l.isEmpty else {
            return nil
        }
        return l
    }
}
