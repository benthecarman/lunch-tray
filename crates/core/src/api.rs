//! The Worker's HTTP API, as the desktop app uses it. Every route takes
//! `Authorization: Bearer <API_TOKEN>`.
//!
//! - `GET /api/state?since=<rev>` returns a [`Delta`]: what changed after
//!   revision `rev`, or `304` when nothing did. `since=0`, or a revision
//!   the Worker cannot answer from, gets the whole list.
//! - `POST /api/ops` takes an [`OpBatch`] and returns one [`OpResult`] per
//!   op, in order. A batch whose id the Worker has seen gets the first
//!   answer again and changes nothing, so a resend after a lost answer is
//!   safe.
//! - `GET /api/ws` opens a WebSocket for pushed changes. The app sends a
//!   [`Hello`] with its revision; the Worker answers with a [`Push`] when
//!   that is behind, and again after every change. `"ping"` gets `"pong"`.
//! - `POST /api/sync` asks for a sync now.
//! - `PUT /api/import` takes a [`Snapshot`] and loads it into an empty
//!   Worker; `?replace=1` replaces what is there. `409` when not empty.

use serde::{Deserialize, Serialize};

use crate::model::{Snapshot, Task};
use crate::ops::{Applied, Op};
use crate::sync::SyncStatus;

pub const STATE: &str = "/api/state";
pub const OPS: &str = "/api/ops";
pub const SYNC: &str = "/api/sync";
pub const IMPORT: &str = "/api/import";
pub const WS: &str = "/api/ws";

/// Keeps a WebSocket alive; the Worker answers `PONG` without waking.
pub const PING: &str = "ping";
pub const PONG: &str = "pong";

/// Every change on the Worker takes the next revision number. Each task
/// row keeps the revision of its last change, so the Worker can answer
/// "what changed after N" from its database alone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delta {
    /// The revision this brings the copy up to. Send it as `since` next.
    pub rev: u64,
    /// `tasks` is the whole list: replace the copy rather than patch it.
    pub full: bool,
    /// Tasks added or changed after `since`.
    pub tasks: Vec<Task>,
    /// Ids of tasks removed after `since`. Apply before `tasks`.
    pub removed: Vec<String>,
    pub next_manual_id: u64,
    pub sync: SyncStatus,
    /// Messages for the user after `since`, oldest first.
    pub notices: Vec<RemoteNotice>,
}

impl Delta {
    /// The whole list as a snapshot, for a `full` delta.
    pub fn snapshot(self) -> Snapshot {
        Snapshot {
            tasks: self.tasks,
            next_manual_id: self.next_manual_id,
        }
    }
}

/// The app's first message on the WebSocket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The revision the app has.
    pub since: u64,
}

/// What the Worker sends on the WebSocket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Push {
    /// Changes after the revision the socket was at. Never `full`.
    Delta(Delta),
    /// The socket is too far behind for a delta: fetch
    /// `/api/state?since=0` and say `Hello` again.
    Resync,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteNotice {
    /// The revision the notice came with.
    pub seq: u64,
    pub error: bool,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpBatch {
    /// Chosen by the sender, the same on every resend.
    pub id: String,
    pub ops: Vec<Op>,
}

pub type OpResult = Result<Applied, String>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushes_carry_their_type_beside_the_delta() {
        // As the Worker writes them: the delta's fields and a `type`.
        let json = r#"{"type":"delta","rev":7,"full":false,"tasks":[],"removed":["manual:1"],
            "next_manual_id":2,"sync":{"in_progress":true,"last_finished":null,"errors":[]},
            "notices":[{"seq":7,"error":false,"text":"New: o/r #5"}]}"#;
        let Push::Delta(d) = serde_json::from_str(json).unwrap() else {
            panic!("not a delta");
        };
        assert_eq!((d.rev, d.removed.len(), d.notices.len()), (7, 1, 1));
        assert!(d.sync.in_progress);
        let resync: Push = serde_json::from_str(r#"{"type":"resync"}"#).unwrap();
        assert_eq!(resync, Push::Resync);
        assert_eq!(
            serde_json::to_string(&Hello { since: 3 }).unwrap(),
            r#"{"since":3}"#
        );
    }
}
