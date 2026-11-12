//! The local sync thread: runs the shared sync pass against the store in
//! `Shared`, sending through ureq. Not used when a Worker does the sync.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use lunch_tray_core::http::{Request, Response, Transport};
use lunch_tray_core::model::Store;
use lunch_tray_core::sync::{Forge, Forgejo, GitHub, SyncHost, SyncOptions, sync_once};

use crate::config::Config;
use crate::shared::{Notice, SharedRef};

/// Tracked items the searches left out are fetched one by one, at most
/// this many per pass.
const MAX_INDIVIDUAL_CHECKS: usize = 60;

pub enum SyncCmd {
    Now,
    Quit,
}

/// Blocking ureq behind the async trait; the sync thread drives it with
/// `pollster`.
pub struct Ureq(ureq::Agent);

impl Ureq {
    pub fn new() -> Self {
        Ureq(
            ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(30)))
                .http_status_as_error(false)
                .build()
                .into(),
        )
    }

    /// Send and read the whole answer.
    pub fn send_blocking(&self, req: Request) -> Result<Response> {
        let mut resp = match (req.method, req.body) {
            ("GET", _) => {
                let mut r = self.0.get(&req.url);
                for (k, v) in &req.headers {
                    r = r.header(k, v);
                }
                r.call()?
            }
            (method, body) => {
                let mut r = match method {
                    "PUT" => self.0.put(&req.url),
                    _ => self.0.post(&req.url),
                };
                for (k, v) in &req.headers {
                    r = r.header(k, v);
                }
                r.send(body.unwrap_or_default())?
            }
        };
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let status = resp.status().as_u16();
        // A 304 has no body, but may still say it is gzipped, which the
        // decoder refuses.
        let body = if matches!(status, 204 | 304) {
            String::new()
        } else {
            resp.body_mut()
                .with_config()
                .limit(64 << 20)
                .read_to_string()
                .context("read the answer")?
        };
        Ok(Response {
            status,
            headers,
            body,
        })
    }
}

#[async_trait(?Send)]
impl Transport for Ureq {
    async fn send(&self, req: Request) -> Result<Response> {
        self.send_blocking(req)
    }
}

struct Host<'a>(&'a SharedRef);

impl SyncHost for Host<'_> {
    fn with_store<R>(&self, f: impl FnOnce(&mut Store) -> R) -> R {
        f(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()).store)
    }

    fn save(&self) {
        let s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = s.store.save() {
            log::error!("save store: {e:#}");
        }
    }

    fn changed(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).notify();
    }

    fn notice(&self, error: bool, text: String) {
        let n = if error {
            Notice::error(text)
        } else {
            Notice::info(text)
        };
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_notice(n);
    }
}

/// Build one forge per configured account. Accounts that cannot be set up
/// (no token) are reported as notices and skipped.
fn build_forges(cfg: &Config, shared: &SharedRef) -> Vec<Box<dyn Forge>> {
    let mut forges: Vec<Box<dyn Forge>> = Vec::new();
    let report = |what: String| shared.lock().unwrap().push_notice(Notice::error(what));
    for acc in &cfg.github {
        match acc.resolve_token() {
            Ok(token) => forges.push(Box::new(GitHub::new(
                &acc.api_url,
                token,
                acc.queries.clone(),
                acc.exclude.clone(),
                Box::new(Ureq::new()),
            ))),
            Err(e) => report(format!("GitHub {}: {e:#}", acc.host())),
        }
    }
    for acc in &cfg.forgejo {
        match acc.resolve_token() {
            Ok(token) => forges.push(Box::new(Forgejo::new(
                &acc.url,
                token,
                acc.queries.clone(),
                acc.exclude.clone(),
                Box::new(Ureq::new()),
            ))),
            Err(e) => report(format!("Forgejo {}: {e:#}", acc.host())),
        }
    }
    forges
}

/// Runs until `Quit` arrives or the sender goes away.
pub fn run_loop(cfg: Config, shared: SharedRef, rx: Receiver<SyncCmd>) {
    let forges = build_forges(&cfg, &shared);
    let interval = Duration::from_secs(cfg.poll_interval_secs.max(15));
    let opts = SyncOptions {
        on_close: cfg.on_close,
        checks: cfg.checks,
        max_individual_checks: MAX_INDIVIDUAL_CHECKS,
    };
    let mut round = 0usize;
    loop {
        if !forges.is_empty() {
            pass(&forges, &shared, opts, round);
        }
        round = round.wrapping_add(1);
        match rx.recv_timeout(interval) {
            Ok(SyncCmd::Now) | Err(RecvTimeoutError::Timeout) => continue,
            Ok(SyncCmd::Quit) | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn pass(forges: &[Box<dyn Forge>], shared: &SharedRef, opts: SyncOptions, round: usize) {
    {
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        s.sync.in_progress = true;
        s.notify();
    }
    let errors = pollster::block_on(sync_once(forges, &Host(shared), opts, round));
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    s.sync.in_progress = false;
    s.sync.last_finished = Some(Utc::now());
    let errors_changed = s.sync.errors != errors;
    s.sync.errors = errors;
    if errors_changed {
        for e in s.sync.errors.clone() {
            s.push_notice(Notice::error(e));
        }
    }
    s.notify();
}
