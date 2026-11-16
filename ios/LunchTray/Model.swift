// The task model as the Worker serves it. Mirrors `crates/core/src/model.rs`
// and `api.rs`; field names follow the Rust serde output.

import Foundation

enum TaskState: String, Codable, Sendable {
    case active, settled, archived
}

enum RemoteKind: String, Codable, Sendable {
    case pullRequest = "pullrequest"
    case issue
}

enum RemoteState: String, Codable, Sendable {
    case open, closed, merged
}

enum MergeState: String, Codable, Sendable {
    case unknown, clean, conflicting, behind, blocked, unstable

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = MergeState(rawValue: raw) ?? .unknown
    }
}

struct Checks: Codable, Equatable, Sendable {
    var passed: UInt32
    var failed: UInt32
    var pending: UInt32
    var total: UInt32
    var merge: MergeState
    var headSha: String
    var checkedAt: Date

    enum CodingKeys: String, CodingKey {
        case passed, failed, pending, total, merge
        case headSha = "head_sha"
        case checkedAt = "checked_at"
    }
}

struct RemoteRef: Codable, Equatable, Sendable {
    var provider: String
    var host: String
    var owner: String
    var repo: String
    var number: UInt64
    var kind: RemoteKind
    var url: String
    var author: String
    var state: RemoteState
    var draft: Bool
    var remoteUpdatedAt: Date
    var inQueries: Bool
    var queryMisses: UInt32
    var checks: Checks?

    enum CodingKeys: String, CodingKey {
        case provider, host, owner, repo, number, kind, url, author, state, draft, checks
        case remoteUpdatedAt = "remote_updated_at"
        case inQueries = "in_queries"
        case queryMisses = "query_misses"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        provider = try c.decode(String.self, forKey: .provider)
        host = try c.decode(String.self, forKey: .host)
        owner = try c.decode(String.self, forKey: .owner)
        repo = try c.decode(String.self, forKey: .repo)
        number = try c.decode(UInt64.self, forKey: .number)
        kind = try c.decode(RemoteKind.self, forKey: .kind)
        url = try c.decode(String.self, forKey: .url)
        author = try c.decode(String.self, forKey: .author)
        state = try c.decode(RemoteState.self, forKey: .state)
        draft = try c.decodeIfPresent(Bool.self, forKey: .draft) ?? false
        remoteUpdatedAt = try c.decode(Date.self, forKey: .remoteUpdatedAt)
        inQueries = try c.decodeIfPresent(Bool.self, forKey: .inQueries) ?? true
        queryMisses = try c.decodeIfPresent(UInt32.self, forKey: .queryMisses) ?? 0
        checks = try c.decodeIfPresent(Checks.self, forKey: .checks)
    }

    var label: String { "\(owner)/\(repo) #\(number)" }
}

/// `{"type":"manual"}` or `{"type":"remote", ...RemoteRef}`.
enum Origin: Codable, Equatable, Sendable {
    case manual
    case remote(RemoteRef)

    private enum Tag: String, CodingKey { case type }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Tag.self)
        switch try c.decode(String.self, forKey: .type) {
        case "remote": self = .remote(try RemoteRef(from: decoder))
        default: self = .manual
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: Tag.self)
        switch self {
        case .manual:
            try c.encode("manual", forKey: .type)
        case .remote(let r):
            try c.encode("remote", forKey: .type)
            try r.encode(to: encoder)
        }
    }
}

struct TrayTask: Codable, Equatable, Identifiable, Sendable {
    var id: String
    var title: String
    var remoteTitle: String?
    var notes: String
    var link: String?
    var project: String
    var state: TaskState
    var pinned: Bool
    var unseen: Bool
    var createdAt: Date
    var updatedAt: Date
    var origin: Origin

    enum CodingKeys: String, CodingKey {
        case id, title, notes, link, project, state, pinned, unseen, origin
        case remoteTitle = "remote_title"
        case createdAt = "created_at"
        case updatedAt = "updated_at"
    }

    init(
        id: String, title: String, remoteTitle: String? = nil, notes: String = "",
        link: String? = nil, project: String, state: TaskState, pinned: Bool = false,
        unseen: Bool = false, createdAt: Date, updatedAt: Date, origin: Origin
    ) {
        self.id = id
        self.title = title
        self.remoteTitle = remoteTitle
        self.notes = notes
        self.link = link
        self.project = project
        self.state = state
        self.pinned = pinned
        self.unseen = unseen
        self.createdAt = createdAt
        self.updatedAt = updatedAt
        self.origin = origin
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        title = try c.decode(String.self, forKey: .title)
        remoteTitle = try c.decodeIfPresent(String.self, forKey: .remoteTitle)
        notes = try c.decodeIfPresent(String.self, forKey: .notes) ?? ""
        link = try c.decodeIfPresent(String.self, forKey: .link)
        project = try c.decode(String.self, forKey: .project)
        state = try c.decode(TaskState.self, forKey: .state)
        pinned = try c.decodeIfPresent(Bool.self, forKey: .pinned) ?? false
        unseen = try c.decodeIfPresent(Bool.self, forKey: .unseen) ?? false
        createdAt = try c.decode(Date.self, forKey: .createdAt)
        updatedAt = try c.decode(Date.self, forKey: .updatedAt)
        origin = try c.decode(Origin.self, forKey: .origin)
    }

    var remote: RemoteRef? {
        if case .remote(let r) = origin { return r }
        return nil
    }

    var isManual: Bool { remote == nil }

    /// The remote item is open, so the store refuses to delete the task.
    var isRunning: Bool { remote?.state == .open }

    var url: URL? {
        let s = remote?.url ?? link
        guard let s, !s.isEmpty else { return nil }
        return URL(string: s)
    }

    /// When the task last moved: the remote item's update time, or the
    /// task's own for manual ones.
    var lastActivity: Date { remote?.remoteUpdatedAt ?? updatedAt }

    var rung: Rung {
        switch state {
        case .archived: .archived
        case .active: .active
        case .settled: pinned ? .active : .settled
        }
    }

    /// Every word of `query` must match the title, owner, repository, or
    /// number. A leading `#` on a number is fine.
    func matches(_ query: String) -> Bool {
        let title = self.title.lowercased()
        let owner = remote?.owner.lowercased() ?? ""
        let repo = remote?.repo.lowercased() ?? ""
        let number = remote.map { String($0.number) } ?? ""
        return query.lowercased().split(whereSeparator: \.isWhitespace).allSatisfy { w in
            let w = String(w)
            let n = w.drop(while: { $0 == "#" })
            return title.contains(w) || owner.contains(w) || repo.contains(w)
                || (!n.isEmpty && number == n)
        }
    }
}

enum Rung { case active, settled, archived }

struct SyncStatus: Codable, Equatable, Sendable {
    var inProgress = false
    var lastFinished: Date?
    var errors: [String] = []

    enum CodingKeys: String, CodingKey {
        case inProgress = "in_progress"
        case lastFinished = "last_finished"
        case errors
    }
}

struct RemoteNotice: Codable, Equatable, Sendable {
    var seq: UInt64
    var error: Bool
    var text: String
}

/// What changed after a revision. See `api::Delta`.
struct Delta: Decodable, Sendable {
    var rev: UInt64
    var full: Bool
    var tasks: [TrayTask]
    var removed: [String]
    var nextManualId: UInt64
    var sync: SyncStatus
    var notices: [RemoteNotice]

    enum CodingKeys: String, CodingKey {
        case rev, full, tasks, removed, sync, notices
        case nextManualId = "next_manual_id"
    }
}

/// What the Worker sends on the WebSocket.
enum Push: Decodable, Sendable {
    case delta(Delta)
    case resync

    private enum Tag: String, CodingKey { case type }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Tag.self)
        switch try c.decode(String.self, forKey: .type) {
        case "delta": self = .delta(try Delta(from: decoder))
        default: self = .resync
        }
    }
}

// MARK: - Dates

enum JSON {
    /// The Worker writes RFC 3339 with up to nine fractional digits, and
    /// `-262143-01-01T00:00:00Z` for a check result marked stale.
    static func parseDate(_ s: String) -> Date? {
        guard !s.hasPrefix("-") else { return .distantPast }
        // Foundation keeps three fractional digits at most.
        var s = s
        if let dot = s.firstIndex(of: "."),
            let end = s[dot...].firstIndex(where: { !$0.isNumber && $0 != "." })
        {
            let frac = s[s.index(after: dot)..<end]
            if frac.count > 3 {
                s.replaceSubrange(s.index(after: dot)..<end, with: frac.prefix(3))
            }
        }
        let style = Date.ISO8601FormatStyle(includingFractionalSeconds: s.contains("."))
        return try? style.parse(s)
    }

    static func formatDate(_ d: Date) -> String {
        // The style truncates to milliseconds; round instead, so a parsed
        // time comes back the same.
        d.addingTimeInterval(0.0005).formatted(Date.ISO8601FormatStyle(includingFractionalSeconds: true))
    }

    static let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.dateDecodingStrategy = .custom { decoder in
            let c = try decoder.singleValueContainer()
            let s = try c.decode(String.self)
            guard let date = parseDate(s) else {
                throw DecodingError.dataCorruptedError(
                    in: c, debugDescription: "bad date \(s)")
            }
            return date
        }
        return d
    }()

    static let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.dateEncodingStrategy = .custom { date, encoder in
            var c = encoder.singleValueContainer()
            try c.encode(date == .distantPast ? "-262143-01-01T00:00:00Z" : formatDate(date))
        }
        return e
    }()
}
