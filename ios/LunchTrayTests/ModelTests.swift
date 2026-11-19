import Foundation
import Testing

@testable import LunchTray

/// A delta as the Worker writes it: nanosecond times, a stale check result
/// at chrono's minimum date, a manual task, and a push's `type`.
private let pushJSON = """
    {"type":"delta","rev":7,"full":false,"removed":["manual:1"],"next_manual_id":3,
     "sync":{"in_progress":false,"last_finished":"2026-11-16T08:20:03.458Z","errors":[]},
     "notices":[{"seq":7,"error":false,"text":"New: o/r #5"}],
     "tasks":[
      {"id":"github:github.com/o/r#5","title":"Fix it","remote_title":"Fix it","notes":"",
       "link":null,"project":"o/r","state":"active","pinned":true,"unseen":true,
       "created_at":"2026-09-21T21:34:57.203161509Z","updated_at":"2026-09-21T23:02:54.284178162Z",
       "origin":{"type":"remote","provider":"github","host":"github.com","owner":"o","repo":"r",
        "number":5,"kind":"pullrequest","url":"https://github.com/o/r/pull/5","author":"me",
        "state":"open","draft":false,"remote_updated_at":"2026-09-21T23:02:25Z",
        "in_queries":true,"query_misses":0,
        "checks":{"passed":3,"failed":0,"pending":1,"total":4,"merge":"conflicting",
         "head_sha":"abc","checked_at":"-262143-01-01T00:00:00Z"}}},
      {"id":"manual:2","title":"Write","project":"Local","state":"settled",
       "created_at":"2026-09-21T21:34:57Z","updated_at":"2026-09-22T21:34:57Z",
       "origin":{"type":"manual"}}
     ]}
    """

@Test func decodesPushedDeltas() throws {
    let push = try JSON.decoder.decode(Push.self, from: Data(pushJSON.utf8))
    guard case .delta(let d) = push else {
        Issue.record("not a delta")
        return
    }
    #expect(d.rev == 7)
    #expect(d.tasks.count == 2)
    let r = try #require(d.tasks[0].remote)
    #expect(r.kind == .pullRequest)
    #expect(r.checks?.merge == .conflicting)
    #expect(r.checks?.checkedAt == .distantPast)
    #expect(d.tasks[0].createdAt.timeIntervalSince1970.rounded() == 1_790_026_497)
    #expect(d.tasks[1].isManual)
    #expect(d.sync.lastFinished != nil)

    let resync = try JSON.decoder.decode(Push.self, from: Data(#"{"type":"resync"}"#.utf8))
    guard case .resync = resync else {
        Issue.record("not a resync")
        return
    }
}

@Test func onlyRecentNoticesAreNews() throws {
    let now = try #require(JSON.parseDate("2026-11-19T17:30:00Z"))
    func notice(_ at: String) throws -> RemoteNotice {
        try JSON.decoder.decode(
            RemoteNotice.self, from: Data(#"{"seq":1,"error":true,"text":"x"\#(at)}"#.utf8))
    }
    #expect(try notice(#","at":"2026-11-19T17:25:00.123Z""#).isNews(now: now))
    #expect(try !notice(#","at":"2026-11-19T17:15:00Z""#).isNews(now: now))
    // From a Worker that does not stamp its notices.
    #expect(try notice("").isNews(now: now))
}

@Test func tasksSurviveTheCache() throws {
    guard case .delta(let d) = try JSON.decoder.decode(Push.self, from: Data(pushJSON.utf8))
    else { return }
    var store = Store()
    store.apply(delta: d)
    let back = try JSON.decoder.decode(Store.self, from: JSON.encoder.encode(store))
    #expect(back == store)
}

@Test func opsEncodeAsTheWorkerReadsThem() throws {
    func json(_ op: Op) throws -> [String: AnyHashable] {
        let data = try JSON.encoder.encode(op)
        return try JSONSerialization.jsonObject(with: data) as! [String: AnyHashable]
    }
    #expect(try json(.settle(id: "manual:1")) == ["op": "settle", "id": "manual:1"])
    #expect(try json(.markAllSeen) == ["op": "mark_all_seen"])
    #expect(
        try json(.setPinned(id: "x", pinned: true)) == ["op": "set_pinned", "id": "x", "pinned": true])
    #expect(try json(.archiveMany(ids: ["a"])) == ["op": "archive_many", "ids": ["a"]])
    let add = try json(.add(title: "T", notes: "", link: nil))
    #expect(add["op"] == AnyHashable("add"))
    #expect(add["link"] == AnyHashable(NSNull()))

    let ops: [Op] = [.add(title: "T", notes: "n", link: "l"), .editManual(id: "m", notes: "", link: nil)]
    let back = try JSON.decoder.decode([Op].self, from: JSON.encoder.encode(ops))
    #expect(back == ops)
}

@Test func opResultsReadSerdeResults() throws {
    let json = #"[{"Ok":{"changed":true,"id":"manual:4","count":1}},{"Err":"nope"}]"#
    let rs = try JSON.decoder.decode([OpResult].self, from: Data(json.utf8))
    #expect(rs[0].applied?.id == "manual:4")
    #expect(rs[1].error == "nope")
}

@Test func transitionsFollowTheCore() throws {
    var s = Store()
    try s.apply(.add(title: " Write ", notes: "", link: " "))
    let id = "manual:1"
    #expect(s.task(id)?.title == "Write")
    #expect(s.task(id)?.link == nil)
    #expect(try s.apply(.setPinned(id: id, pinned: true)))
    #expect(try s.apply(.settle(id: id)))
    #expect(s.task(id)?.pinned == false)
    #expect(try !s.apply(.settle(id: id)))
    #expect(throws: OpError.self) { try s.apply(.setPinned(id: id, pinned: true)) }
    #expect(throws: OpError.self) { try s.apply(.add(title: " ", notes: "", link: nil)) }
    #expect(try s.apply(.reopen(id: id)))
    #expect(s.sections().active.map(\.id) == [id])
    #expect(try s.apply(.delete(id: id)))
    #expect(s.tasks.isEmpty)
}

@Test func runningTasksCannotBeDeleted() throws {
    guard case .delta(let d) = try JSON.decoder.decode(Push.self, from: Data(pushJSON.utf8))
    else { return }
    var s = Store()
    s.apply(delta: d)
    let id = "github:github.com/o/r#5"
    #expect(throws: OpError.self) { try s.apply(.delete(id: id)) }
    #expect(try s.apply(.archive(id: id)))
    #expect(s.sections().archived.map(\.id) == [id])
}

@Test func searchMatchesEveryWord() throws {
    guard case .delta(let d) = try JSON.decoder.decode(Push.self, from: Data(pushJSON.utf8))
    else { return }
    let t = d.tasks[0]
    #expect(t.matches("o r 5"))
    #expect(t.matches("FIX #5"))
    #expect(!t.matches("fix 6"))
}
