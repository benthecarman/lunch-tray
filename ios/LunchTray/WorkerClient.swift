// The Worker's HTTP API and WebSocket. See `crates/core/src/api.rs`.

import Foundation

struct WorkerConfig: Equatable, Sendable {
    var url: String
    var token: String

    var isComplete: Bool { baseURL != nil && !token.isEmpty }

    var baseURL: URL? {
        var s = url.trimmingCharacters(in: .whitespacesAndNewlines)
        while s.hasSuffix("/") { s.removeLast() }
        guard !s.isEmpty else { return nil }
        if !s.contains("://") { s = "https://" + s }
        guard let u = URL(string: s), u.host() != nil else { return nil }
        return u
    }
}

enum WorkerError: Error, LocalizedError {
    case badURL
    case unauthorized
    case status(Int, String)
    case badAnswer(String)

    var errorDescription: String? {
        switch self {
        case .badURL: "The Worker URL is not valid."
        case .unauthorized: "The Worker refused the token."
        case let .status(code, body):
            body.isEmpty ? "HTTP status \(code)" : "HTTP status \(code): \(body)"
        case let .badAnswer(what): "Could not read the Worker's answer: \(what)"
        }
    }
}

struct WorkerClient: Sendable {
    let base: URL
    let token: String
    let session: URLSession

    init(config: WorkerConfig, session: URLSession = .shared) throws {
        guard let base = config.baseURL else { throw WorkerError.badURL }
        self.base = base
        self.token = config.token
        self.session = session
    }

    private func request(_ method: String, _ path: String, query: String? = nil, body: Data? = nil)
        -> URLRequest
    {
        var comps = URLComponents(
            url: base.appending(path: path), resolvingAgainstBaseURL: false)!
        comps.percentEncodedQuery = query
        var req = URLRequest(url: comps.url!, timeoutInterval: 20)
        req.httpMethod = method
        req.cachePolicy = .reloadIgnoringLocalCacheData
        req.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        if let body {
            req.httpBody = body
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        return req
    }

    private func send(_ req: URLRequest) async throws -> (Data, Int) {
        let (data, resp) = try await session.data(for: req)
        let code = (resp as? HTTPURLResponse)?.statusCode ?? 0
        switch code {
        case 401: throw WorkerError.unauthorized
        case 400...:
            throw WorkerError.status(code, String(decoding: data.prefix(300), as: UTF8.self))
        default: return (data, code)
        }
    }

    /// What changed after revision `since`, or nil when nothing did.
    func changes(since: UInt64) async throws -> Delta? {
        let (data, code) = try await send(request("GET", "/api/state", query: "since=\(since)"))
        if code == 304 { return nil }
        do {
            return try JSON.decoder.decode(Delta.self, from: data)
        } catch {
            throw WorkerError.badAnswer("\(error)")
        }
    }

    func ops(_ batch: OpBatch) async throws -> [OpResult] {
        let body = try JSON.encoder.encode(batch)
        let (data, _) = try await send(request("POST", "/api/ops", body: body))
        do {
            return try JSON.decoder.decode([OpResult].self, from: data)
        } catch {
            throw WorkerError.badAnswer("\(error)")
        }
    }

    func syncNow() async throws {
        _ = try await send(request("POST", "/api/sync", body: Data("{}".utf8)))
    }

    /// The socket the Worker pushes changes down. Not yet resumed.
    func socket() -> URLSessionWebSocketTask {
        var req = request("GET", "/api/ws")
        var comps = URLComponents(url: req.url!, resolvingAgainstBaseURL: false)!
        comps.scheme = comps.scheme == "http" ? "ws" : "wss"
        req.url = comps.url
        return session.webSocketTask(with: req)
    }
}
