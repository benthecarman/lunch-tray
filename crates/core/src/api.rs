//! The Worker's HTTP API, as the desktop app uses it. Every route takes
//! `Authorization: Bearer <API_TOKEN>`.
//!
//! - `GET /api/state` returns [`State`]. Send the last `ETag` as
//!   `If-None-Match` to get `304` while nothing changed.
//! - `POST /api/ops` takes an [`OpBatch`] and returns one [`OpResult`] per
//!   op, in order. A batch whose id the Worker has seen gets the first
//!   answer again and changes nothing, so a resend after a lost answer is
//!   safe.
//! - `POST /api/sync` asks for a sync now.
//! - `PUT /api/import` takes a [`Snapshot`] and loads it into an empty
//!   Worker; `?replace=1` replaces what is there. `409` when not empty.

use serde::{Deserialize, Serialize};

use crate::model::Snapshot;
use crate::ops::{Applied, Op};
use crate::sync::SyncStatus;

pub const STATE: &str = "/api/state";
pub const OPS: &str = "/api/ops";
pub const SYNC: &str = "/api/sync";
pub const IMPORT: &str = "/api/import";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Changes when the Worker restarts; `seq` counts from there.
    pub boot: u64,
    pub snapshot: Snapshot,
    pub sync: SyncStatus,
    /// Recent messages for the user, oldest first.
    pub notices: Vec<RemoteNotice>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteNotice {
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
