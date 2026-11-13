//! The link to a Lunch Tray Worker. The Worker owns the tasks and runs the
//! sync; this thread sends it the changes made here and brings its changes
//! back into `Shared`, where the window and the tray read them as before.
//!
//! A change shows at once: `Shared::apply` runs the op on the local copy
//! and queues it here. The Worker applies the same op with the same rules.
//!
//! The thread keeps `base`, the Worker's list as of revision `rev`, and
//! asks only for what changed after that. When nothing did, the Worker
//! answers `304` from a single row. After a change, the window's copy is
//! `base` again, with the ops the Worker has not confirmed yet applied on
//! top, so nothing flickers back.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use lunch_tray_core::api::{self, Delta, OpBatch, OpResult};
use lunch_tray_core::http::{Request, Response};
use lunch_tray_core::model::{Snapshot, Store};
use lunch_tray_core::ops::Op;
use lunch_tray_core::sync::SyncStatus;

use crate::config::RemoteConfig;
use crate::shared::{Notice, SharedRef};
use crate::sync::Ureq;

pub enum RemoteCmd {
    Op(Op),
    SyncNow,
    /// Check for changes now, say because a window opened.
    Refresh,
    Quit,
}

pub struct Client {
    base: String,
    token: String,
    http: Ureq,
}

impl Client {
    pub fn new(cfg: &RemoteConfig) -> Result<Self> {
        Ok(Client {
            base: cfg.url.trim_end_matches('/').to_string(),
            token: cfg.resolve_token()?,
            http: Ureq::new(),
        })
    }

    fn send(&self, method: &'static str, path: &str, body: Option<String>) -> Result<Response> {
        let mut headers = vec![(
            "Authorization".to_string(),
            format!("Bearer {}", self.token),
        )];
        if body.is_some() {
            headers.push(("Content-Type".into(), "application/json".into()));
        }
        let url = format!("{}{path}", self.base);
        let resp = self
            .http
            .send_blocking(Request {
                method,
                url: url.clone(),
                headers,
                body,
            })
            .with_context(|| format!("{method} {url}"))?;
        match resp.status {
            401 => bail!("the Worker refused the token"),
            s if s >= 400 && s != 409 => bail!("{method} {path}: http status {s}: {}", resp.body),
            _ => Ok(resp),
        }
    }

    /// What changed after revision `since`, or `None` when nothing did.
    fn changes(&self, since: u64) -> Result<Option<Delta>> {
        let resp = self.send("GET", &format!("{}?since={since}", api::STATE), None)?;
        if resp.status == 304 {
            return Ok(None);
        }
        let delta = serde_json::from_str(&resp.body).context("decode the Worker's changes")?;
        Ok(Some(delta))
    }

    fn ops(&self, batch: &OpBatch) -> Result<Vec<OpResult>> {
        let resp = self.send("POST", api::OPS, Some(serde_json::to_string(batch)?))?;
        serde_json::from_str(&resp.body).context("decode the Worker's answer")
    }

    fn sync_now(&self) -> Result<()> {
        self.send("POST", api::SYNC, Some("{}".into()))?;
        Ok(())
    }

    /// Load `snap` into the Worker. Without `replace`, only into an empty one.
    pub fn import(&self, snap: &Snapshot, replace: bool) -> Result<()> {
        let path = if replace {
            format!("{}?replace=1", api::IMPORT)
        } else {
            api::IMPORT.to_string()
        };
        let resp = self.send("PUT", &path, Some(serde_json::to_string(snap)?))?;
        if resp.status == 409 {
            bail!("the Worker already has tasks; use --upload --replace to overwrite them");
        }
        Ok(())
    }
}

/// How often to check the Worker for changes.
#[derive(Clone, Copy, Debug)]
pub struct Polling {
    /// While a window is open.
    pub open: Duration,
    /// With only the tray icon.
    pub idle: Duration,
}

/// Runs until `Quit` arrives or the sender goes away.
pub fn run(client: Client, polling: Polling, shared: SharedRef, rx: Receiver<RemoteCmd>) {
    let mut link = Link::new(client, shared);
    let mut sync_now = false;
    let mut wait = Duration::ZERO;
    loop {
        match rx.recv_timeout(wait) {
            Ok(RemoteCmd::Quit) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(cmd) => link.take(cmd, &mut sync_now),
            Err(RecvTimeoutError::Timeout) => {}
        }
        if !link.drain(&rx, &mut sync_now) {
            return;
        }
        wait = if link.shared.lock().unwrap().window_mode.is_some() {
            polling.open
        } else {
            polling.idle
        };
        if let Err(e) = link.round(&mut sync_now) {
            link.went_offline(e);
            continue;
        }
        link.came_online();
        // Ops that arrived during the round go out right away.
        if !link.pending.is_empty() {
            wait = Duration::ZERO;
        }
    }
}

struct Link {
    client: Client,
    shared: SharedRef,
    /// The Worker's list as of `rev`, without local changes.
    base: Store,
    /// The Worker revision `base` is at; 0 before the first answer.
    rev: u64,
    /// The Worker's own sync status, to restore after being offline.
    worker_sync: SyncStatus,
    /// Ops sent but not yet answered. A failed send is retried with the
    /// same batch, so the Worker can tell it already applied them.
    in_flight: Option<OpBatch>,
    /// Ops applied here and not yet sent.
    pending: VecDeque<Op>,
    /// Why the Worker cannot be reached, while it cannot.
    offline: Option<String>,
}

impl Link {
    fn new(client: Client, shared: SharedRef) -> Self {
        Link {
            client,
            shared,
            base: Store::in_memory(),
            rev: 0,
            worker_sync: SyncStatus::default(),
            in_flight: None,
            pending: VecDeque::new(),
            offline: None,
        }
    }

    fn take(&mut self, cmd: RemoteCmd, sync_now: &mut bool) {
        match cmd {
            RemoteCmd::Op(op) => self.pending.push_back(op),
            RemoteCmd::SyncNow => *sync_now = true,
            RemoteCmd::Refresh | RemoteCmd::Quit => {}
        }
    }

    /// Queue whatever arrived. False when it is time to stop.
    fn drain(&mut self, rx: &Receiver<RemoteCmd>, sync_now: &mut bool) -> bool {
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, RemoteCmd::Quit) {
                return false;
            }
            self.take(cmd, sync_now);
        }
        true
    }

    /// Send queued ops, ask for a sync if wanted, then fetch the changes.
    fn round(&mut self, sync_now: &mut bool) -> Result<()> {
        if self.in_flight.is_none() && !self.pending.is_empty() {
            self.in_flight = Some(OpBatch {
                id: batch_id(),
                ops: self.pending.drain(..).collect(),
            });
        }
        if let Some(batch) = &self.in_flight {
            let results = self.client.ops(batch)?;
            self.in_flight = None;
            for r in results {
                if let Err(e) = r {
                    // The Worker refused: its list wins with the next changes.
                    self.notice(Notice::error(e));
                }
            }
        }
        if *sync_now {
            self.client.sync_now()?;
            *sync_now = false;
        }
        if let Some(delta) = self.client.changes(self.rev)? {
            self.take_delta(delta);
        }
        Ok(())
    }

    fn take_delta(&mut self, delta: Delta) {
        let first = self.rev == 0;
        self.rev = delta.rev;
        self.worker_sync = delta.sync.clone();
        let Delta {
            full,
            tasks,
            removed,
            next_manual_id,
            sync,
            notices,
            ..
        } = delta;
        if full {
            self.base.replace_with(Snapshot {
                tasks,
                next_manual_id,
            });
        } else {
            self.base.apply_delta(tasks, &removed, next_manual_id);
        }
        let mut s = self.shared.lock().unwrap();
        s.store.replace_with(self.base.snapshot());
        // Changes the Worker has not confirmed yet go on top.
        let unconfirmed = self.in_flight.iter().flat_map(|b| b.ops.iter());
        for op in unconfirmed.chain(self.pending.iter()) {
            let _ = op.apply(&mut s.store);
        }
        s.sync = sync;
        if !first {
            for n in notices {
                s.push_notice(if n.error {
                    Notice::error(n.text)
                } else {
                    Notice::info(n.text)
                });
            }
        }
        s.save_store();
        s.notify();
    }

    fn notice(&self, n: Notice) {
        self.shared.lock().unwrap().push_notice(n);
    }

    fn went_offline(&mut self, e: anyhow::Error) {
        let why = format!("Worker unreachable: {e:#}");
        log::warn!("{why}");
        let mut s = self.shared.lock().unwrap();
        if self.offline.is_none() {
            s.push_notice(Notice::error(why.clone()));
        }
        s.sync.errors = vec![why.clone()];
        s.sync.in_progress = false;
        s.notify();
        self.offline = Some(why);
    }

    fn came_online(&mut self) {
        if self.offline.take().is_some() {
            log::info!("Worker reachable again");
            let mut s = self.shared.lock().unwrap();
            s.sync = self.worker_sync.clone();
            s.notify();
        }
    }
}

/// Unique enough to tell this app's batches apart: the time, the process,
/// and a counter.
fn batch_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{nanos:x}-{:x}-{:x}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// `--upload`: send the local task file to the Worker.
pub fn upload(cfg: &RemoteConfig, local: &Store, replace: bool) -> Result<usize> {
    let snap = local.snapshot();
    Client::new(cfg)?.import(&snap, replace)?;
    Ok(snap.tasks.len())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::shared::Shared;

    /// Against a running Worker, such as `wrangler dev`:
    /// `LUNCH_TRAY_TEST_WORKER=http://127.0.0.1:8787 LUNCH_TRAY_REMOTE_TOKEN=... cargo test -- --ignored`
    #[test]
    #[ignore]
    fn ops_reach_the_worker_and_come_back() {
        let url = std::env::var("LUNCH_TRAY_TEST_WORKER").expect("LUNCH_TRAY_TEST_WORKER");
        let cfg = RemoteConfig {
            url,
            token: None,
            token_env: None,
            poll_secs: 1,
            idle_poll_secs: 1,
        };
        let (main_tx, _) = channel();
        let (sync_tx, _) = channel();
        let (remote_tx, remote_rx) = channel();
        let shared = Arc::new(Mutex::new(Shared {
            store: Store::in_memory(),
            notices: Vec::new(),
            sync: Default::default(),
            system_theme: crate::theme::SystemTheme::Light,
            ctx: None,
            window_mode: None,
            ui_requests: Vec::new(),
            activation_token: None,
            window: None,
            tray: crate::tray::TrayLink::new(),
            main_tx,
            sync_tx,
            remote: Some(remote_tx.clone()),
        }));
        let mut link = Link::new(Client::new(&cfg).unwrap(), shared.clone());
        let mut sync_now = false;
        link.round(&mut sync_now).unwrap();
        let before = shared.lock().unwrap().store.tasks().len();
        let rev = link.rev;
        assert!(rev > 0);
        // Nothing changed: a 304, not a list.
        assert!(link.client.changes(rev).unwrap().is_none());

        let title = format!("remote test {}", chrono::Utc::now().timestamp_millis());
        let id = shared
            .lock()
            .unwrap()
            .apply(Op::Add {
                title: title.clone(),
                notes: String::new(),
                link: None,
            })
            .unwrap()
            .id
            .unwrap();
        assert!(link.drain(&remote_rx, &mut sync_now));
        assert_eq!(link.pending.len(), 1);
        link.round(&mut sync_now).unwrap();
        assert!(link.pending.is_empty());
        assert!(link.rev > rev);
        {
            let s = shared.lock().unwrap();
            assert_eq!(s.store.tasks().len(), before + 1);
            assert_eq!(s.store.get(&id).unwrap().title, title);
        }

        // A resend of an answered batch changes nothing.
        let again = OpBatch {
            id: format!("replayed-{}", chrono::Utc::now().timestamp_millis()),
            ops: vec![Op::Add {
                title: "added once".into(),
                notes: String::new(),
                link: None,
            }],
        };
        let first = link.client.ops(&again).unwrap();
        assert_eq!(link.client.ops(&again).unwrap(), first);
        let added = first[0].as_ref().unwrap().id.clone().unwrap();
        link.round(&mut sync_now).unwrap();
        let count = |title: &str| {
            let s = shared.lock().unwrap();
            s.store.tasks().iter().filter(|t| t.title == title).count()
        };
        assert_eq!(count("added once"), 1);
        shared.lock().unwrap().run(Op::Delete { id: added });

        shared.lock().unwrap().run(Op::Delete { id: id.clone() });
        assert!(link.drain(&remote_rx, &mut sync_now));
        link.round(&mut sync_now).unwrap();
        assert!(shared.lock().unwrap().store.get(&id).is_none());
        assert_eq!(shared.lock().unwrap().store.tasks().len(), before);
    }
}
