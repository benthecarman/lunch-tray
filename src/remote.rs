//! The link to a Lunch Tray Worker. The Worker owns the tasks and runs the
//! sync; this thread sends it the changes made here and brings its changes
//! back into `Shared`, where the window and the tray read them as before.
//!
//! A change shows at once: `Shared::apply` runs the op on the local copy
//! and queues it here. The Worker applies the same op with the same rules.
//!
//! The thread keeps `base`, the Worker's list as of revision `rev`, and
//! only ever takes what changed after that. Normally the Worker pushes
//! each change down a WebSocket the moment it happens; while there is no
//! socket, the thread polls over HTTP, where an unchanged list is an empty
//! `304`. After a change, the window's copy is `base` again, with the ops
//! the Worker has not confirmed yet applied on top, so nothing flickers
//! back.

use std::collections::VecDeque;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use lunch_tray_core::api::{self, Delta, Hello, OpBatch, OpResult, Push};
use lunch_tray_core::http::{Request, Response};
use lunch_tray_core::model::{Snapshot, Store};
use lunch_tray_core::ops::Op;
use lunch_tray_core::sync::SyncStatus;

use crate::config::RemoteConfig;
use crate::shared::{Notice, SharedRef};
use crate::sync::Ureq;

/// With a socket, how long a read waits, so commands are not held up.
const TICK: Duration = Duration::from_millis(250);
/// How often to ping the Worker over the socket.
const PING_EVERY: Duration = Duration::from_secs(30);
/// A socket this quiet, pings included, is dead.
const SILENCE_LIMIT: Duration = Duration::from_secs(75);
/// First wait before trying a socket again; it doubles up to `MAX_BACKOFF`.
const MIN_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(300);

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

    /// Open the WebSocket the Worker pushes changes down.
    fn connect(&self) -> Result<Socket> {
        use tungstenite::client::IntoClientRequest;
        let url = self
            .base
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        let mut req = format!("{url}{}", api::WS).into_client_request()?;
        req.headers_mut()
            .insert("Authorization", format!("Bearer {}", self.token).parse()?);
        let uri = req.uri().clone();
        let host = uri.host().context("no host in the Worker URL")?;
        let port = uri
            .port_u16()
            .unwrap_or(if uri.scheme_str() == Some("wss") {
                443
            } else {
                80
            });
        let addr = (host, port)
            .to_socket_addrs()?
            .next()
            .with_context(|| format!("no address for {host}"))?;
        let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(10))?;
        tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
        tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
        let (ws, _) = tungstenite::client_tls_with_config(req, tcp, None, None)
            .map_err(|e| anyhow!("open the socket: {e}"))?;
        // From here, a read waits a tick at most.
        tcp_of(&ws)?.set_read_timeout(Some(TICK))?;
        let now = Instant::now();
        Ok(Socket {
            ws,
            opened: now,
            heard: now,
            pinged: now,
        })
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

type Ws = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

fn tcp_of(ws: &Ws) -> Result<&TcpStream> {
    use tungstenite::stream::MaybeTlsStream;
    match ws.get_ref() {
        MaybeTlsStream::Plain(s) => Ok(s),
        MaybeTlsStream::Rustls(s) => Ok(s.get_ref()),
        _ => bail!("unexpected socket kind"),
    }
}

/// The WebSocket the Worker pushes changes down.
struct Socket {
    ws: Ws,
    opened: Instant,
    /// When the Worker last sent anything, pongs included.
    heard: Instant,
    pinged: Instant,
}

impl Socket {
    fn hello(&mut self, since: u64) -> Result<()> {
        let hello = serde_json::to_string(&Hello { since })?;
        self.ws.send(tungstenite::Message::text(hello))?;
        Ok(())
    }

    /// One message, or `None` after a tick without one.
    fn read(&mut self) -> Result<Option<Push>> {
        use tungstenite::Message;
        let msg = match self.ws.read() {
            Ok(msg) => msg,
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        self.heard = Instant::now();
        match msg {
            Message::Text(t) if t.as_str() == api::PONG => Ok(None),
            Message::Text(t) => Ok(Some(
                serde_json::from_str(t.as_str()).context("decode a pushed change")?,
            )),
            Message::Close(_) => bail!("the Worker closed the socket"),
            _ => Ok(None),
        }
    }

    /// Ping now and then; fail when the Worker has gone quiet.
    fn keepalive(&mut self) -> Result<()> {
        if self.heard.elapsed() > SILENCE_LIMIT {
            bail!("no word from the Worker for {}s", SILENCE_LIMIT.as_secs());
        }
        if self.pinged.elapsed() >= PING_EVERY {
            self.ws.send(tungstenite::Message::text(api::PING))?;
            self.pinged = Instant::now();
        }
        Ok(())
    }
}

/// How often to poll for changes while there is no socket.
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
    let mut socket: Option<Socket> = None;
    let mut retry_at = Instant::now();
    let mut backoff = MIN_BACKOFF;
    let mut wait = Duration::ZERO;
    loop {
        // With a socket, its read further down does the waiting.
        let timeout = if socket.is_some() {
            Duration::ZERO
        } else {
            wait
        };
        match rx.recv_timeout(timeout) {
            Ok(RemoteCmd::Quit) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(cmd) => link.take(cmd, &mut sync_now),
            Err(RecvTimeoutError::Timeout) => {}
        }
        if !link.drain(&rx, &mut sync_now) {
            return;
        }
        // Ops and sync requests go over HTTP; changes too, without a socket.
        if let Err(e) = link.round(&mut sync_now, socket.is_none()) {
            link.went_offline(e);
            socket = None;
            wait = polling.idle;
            continue;
        }
        link.came_online();

        if socket.is_none() && link.rev != 0 && Instant::now() >= retry_at {
            match link.client.connect().and_then(|mut s| {
                s.hello(link.rev)?;
                Ok(s)
            }) {
                Ok(s) => {
                    log::info!("the Worker pushes changes over a socket");
                    socket = Some(s);
                }
                Err(e) => {
                    log::warn!("no socket ({e:#}); polling");
                    retry_at = Instant::now() + backoff;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
        if let Some(s) = socket.as_mut() {
            let r = s
                .read()
                .and_then(|push| match push {
                    Some(push) => link.take_push(push, s),
                    None => Ok(()),
                })
                .and_then(|()| s.keepalive());
            if let Err(e) = r {
                log::info!("socket closed: {e:#}");
                // A socket that keeps dropping right away waits longer.
                if s.opened.elapsed() > Duration::from_secs(60) {
                    backoff = MIN_BACKOFF;
                }
                retry_at = Instant::now() + backoff;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                socket = None;
                // Catch up over HTTP right away.
                wait = Duration::ZERO;
            }
            continue;
        }
        wait = if !link.pending.is_empty() {
            // Ops that arrived during the round go out right away.
            Duration::ZERO
        } else if link.shared.lock().unwrap().window_mode.is_some() {
            polling.open
        } else {
            polling.idle
        };
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
    /// The newest Worker notice shown. A reconnect can repeat some.
    notice_seq: u64,
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
            notice_seq: 0,
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

    /// Send queued ops, ask for a sync if wanted, then, with `fetch`, take
    /// the changes.
    fn round(&mut self, sync_now: &mut bool, fetch: bool) -> Result<()> {
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
        if fetch && let Some(delta) = self.client.changes(self.rev)? {
            self.take_delta(delta);
        }
        Ok(())
    }

    fn take_push(&mut self, push: Push, socket: &mut Socket) -> Result<()> {
        match push {
            Push::Delta(delta) => self.take_delta(delta),
            Push::Resync => {
                log::info!("the Worker asked for a fresh start");
                let full = self.client.changes(0)?.context("no list from the Worker")?;
                self.take_delta(full);
                socket.hello(self.rev)?;
            }
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
        for n in notices {
            if n.seq <= self.notice_seq {
                continue;
            }
            self.notice_seq = n.seq;
            if !first {
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

    /// Against a running Worker, such as `wrangler dev`, one test at a time:
    /// `LUNCH_TRAY_TEST_WORKER=http://127.0.0.1:8787 LUNCH_TRAY_REMOTE_TOKEN=...
    /// cargo test -- --ignored --test-threads=1`
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
        link.round(&mut sync_now, true).unwrap();
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
        link.round(&mut sync_now, true).unwrap();
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
        link.round(&mut sync_now, true).unwrap();
        let count = |title: &str| {
            let s = shared.lock().unwrap();
            s.store.tasks().iter().filter(|t| t.title == title).count()
        };
        assert_eq!(count("added once"), 1);
        shared.lock().unwrap().run(Op::Delete { id: added });

        shared.lock().unwrap().run(Op::Delete { id: id.clone() });
        assert!(link.drain(&remote_rx, &mut sync_now));
        link.round(&mut sync_now, true).unwrap();
        assert!(shared.lock().unwrap().store.get(&id).is_none());
        assert_eq!(shared.lock().unwrap().store.tasks().len(), before);
    }

    /// Against a running Worker, like `ops_reach_the_worker_and_come_back`.
    #[test]
    #[ignore]
    fn the_worker_pushes_changes_down_the_socket() {
        let url = std::env::var("LUNCH_TRAY_TEST_WORKER").expect("LUNCH_TRAY_TEST_WORKER");
        let cfg = RemoteConfig {
            url,
            token: None,
            token_env: None,
            poll_secs: 1,
            idle_poll_secs: 1,
        };
        let client = Client::new(&cfg).unwrap();
        let rev = client.changes(0).unwrap().unwrap().rev;
        let mut socket = client.connect().unwrap();
        socket.hello(rev).unwrap();

        let title = format!("pushed {}", chrono::Utc::now().timestamp_millis());
        let batch = OpBatch {
            id: batch_id(),
            ops: vec![Op::Add {
                title: title.clone(),
                notes: String::new(),
                link: None,
            }],
        };
        let id = client.ops(&batch).unwrap()[0]
            .as_ref()
            .unwrap()
            .id
            .clone()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let delta = loop {
            assert!(Instant::now() < deadline, "no push within 10s");
            if let Some(Push::Delta(d)) = socket.read().unwrap() {
                break d;
            }
        };
        assert!(!delta.full);
        assert!(delta.rev > rev);
        assert!(delta.tasks.iter().any(|t| t.title == title));

        // Pings come back as pongs, which count as hearing from it.
        socket.pinged = Instant::now() - PING_EVERY;
        let before = socket.heard;
        socket.keepalive().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while socket.heard == before {
            assert!(Instant::now() < deadline, "no pong within 5s");
            // Other changes may arrive meanwhile.
            socket.read().unwrap();
        }

        let delete = OpBatch {
            id: batch_id(),
            ops: vec![Op::Delete { id }],
        };
        client.ops(&delete).unwrap();
    }
}
