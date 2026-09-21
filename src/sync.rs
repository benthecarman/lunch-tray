//! Remote providers (GitHub, Forgejo) and the background sync loop.

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::config::{Config, ForgejoAccount, GithubAccount, is_excluded};
use crate::model::{Provider, RemoteItem, RemoteKind, RemoteRef, RemoteState, SyncEvent};
use crate::shared::{Notice, SharedRef};

const USER_AGENT: &str = concat!("lunch-tray/", env!("CARGO_PKG_VERSION"));
const MAX_PAGES: usize = 5;
const MAX_INDIVIDUAL_CHECKS: usize = 60;

pub enum SyncCmd {
    Now,
    Quit,
}

pub trait Forge: Send {
    fn provider(&self) -> Provider;
    fn host(&self) -> String;
    /// Every open item that matches the configured queries.
    fn fetch_open(&self) -> Result<Vec<RemoteItem>>;
    /// One item by number, whatever its state.
    fn fetch_one(&self, owner: &str, repo: &str, number: u64) -> Result<RemoteItem>;
    /// Repository patterns to ignore.
    fn exclude(&self) -> &[String];
}

/// One HTTP client per account. Error statuses come back as responses so
/// the proof-of-work gate can be read before deciding what failed.
struct Http {
    agent: ureq::Agent,
    /// Cookie earned from a proof-of-work gate, if the host has one.
    gate_cookie: Mutex<Option<String>>,
}

impl Http {
    fn new() -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .user_agent(USER_AGENT)
            .http_status_as_error(false)
            .build()
            .into();
        Http {
            agent,
            gate_cookie: Mutex::new(None),
        }
    }

    /// GET `url` with `headers` and decode JSON. A 403 carrying a
    /// proof-of-work challenge page is solved once, then the request is
    /// retried with the earned cookie.
    fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        headers: &[(&str, String)],
    ) -> Result<T> {
        for attempt in 0..2 {
            let mut req = self.agent.get(url);
            for (k, v) in headers {
                req = req.header(*k, v);
            }
            let cookie = self
                .gate_cookie
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(c) = &cookie {
                req = req.header("Cookie", c);
            }
            let mut resp = req.call().with_context(|| format!("GET {url}"))?;
            let status = resp.status().as_u16();
            let html = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ct| ct.contains("text/html"));
            if status == 403 && html && attempt == 0 {
                let body = resp
                    .body_mut()
                    .read_to_string()
                    .with_context(|| format!("read {url}"))?;
                if let Some(challenge) = pow::parse(&body) {
                    let cookie = pow::solve(&challenge);
                    log::info!(
                        "solved the proof-of-work gate for {url} at difficulty {}",
                        challenge.difficulty
                    );
                    *self.gate_cookie.lock().unwrap_or_else(|e| e.into_inner()) = Some(cookie);
                    continue;
                }
                bail!("GET {url}: http status 403");
            }
            if status >= 400 {
                return Err(ureq::Error::StatusCode(status)).with_context(|| format!("GET {url}"));
            }
            return resp
                .body_mut()
                .read_json::<T>()
                .with_context(|| format!("decode {url}"));
        }
        bail!("GET {url}: the proof-of-work gate did not accept the answer")
    }
}

/// Some self-hosted forges sit behind a bot gate that answers with an HTML
/// page asking the client for a SHA-1 proof of work: find a nonce so that
/// `sha1(challenge + ":" + nonce)` starts with `difficulty` zero bits, then
/// present `challenge:nonce` in a cookie. This does what a browser would.
mod pow {
    pub struct Challenge {
        pub difficulty: u32,
        pub challenge: String,
        pub cookie_name: String,
    }

    /// Value of a `const NAME = ...;` line in the page's script.
    fn js_const<'a>(html: &'a str, name: &str) -> Option<&'a str> {
        let key = format!("const {name}");
        let start = html.find(&key)? + key.len();
        let rest = &html[start..];
        let eq = rest.find('=')?;
        let rest = &rest[eq + 1..];
        let end = rest.find(';')?;
        Some(rest[..end].trim().trim_matches('"'))
    }

    pub fn parse(html: &str) -> Option<Challenge> {
        let difficulty = js_const(html, "difficulty")?.parse().ok()?;
        let challenge = js_const(html, "challenge")?.to_string();
        let cookie_name = js_const(html, "tokenCookie")?.to_string();
        if challenge.is_empty() || cookie_name.is_empty() || difficulty > 40 {
            return None;
        }
        Some(Challenge {
            difficulty,
            challenge,
            cookie_name,
        })
    }

    fn leading_zero_bits(bytes: &[u8]) -> u32 {
        let mut n = 0;
        for b in bytes {
            if *b == 0 {
                n += 8;
            } else {
                n += b.leading_zeros();
                break;
            }
        }
        n
    }

    /// Returns the `Cookie` header value.
    pub fn solve(c: &Challenge) -> String {
        let mut nonce: u64 = 0;
        loop {
            let data = format!("{}:{nonce}", c.challenge);
            let digest =
                ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, data.as_bytes());
            if leading_zero_bits(digest.as_ref()) >= c.difficulty {
                return format!("{}={}", c.cookie_name, super::urlencode(&data));
            }
            nonce += 1;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const PAGE: &str = r#"<script>(function(){
    const difficulty   = 8;
    const challenge    = "XY811qPJFlefxx+HbqLBb3zuGkI=";
    const tokenCookie  = "poW_token_v4";
    const includeUI    = true;
})();</script>"#;

        #[test]
        fn parses_and_solves_a_challenge_page() {
            let c = parse(PAGE).unwrap();
            assert_eq!(c.difficulty, 8);
            assert_eq!(c.cookie_name, "poW_token_v4");
            let cookie = solve(&c);
            let (name, value) = cookie.split_once('=').unwrap();
            assert_eq!(name, "poW_token_v4");
            assert!(value.starts_with("XY811qPJFlefxx%2BHbqLBb3zuGkI%3D%3A"));
            let nonce: u64 = value.rsplit("%3A").next().unwrap().parse().unwrap();
            let digest = ring::digest::digest(
                &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
                format!("{}:{nonce}", c.challenge).as_bytes(),
            );
            assert!(leading_zero_bits(digest.as_ref()) >= 8);
        }

        #[test]
        fn ignores_pages_without_a_challenge() {
            assert!(parse("<html>403 Forbidden</html>").is_none());
        }
    }
}

fn parse_time(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

// ----- GitHub ---------------------------------------------------------------

pub struct GitHub {
    api_url: String,
    host: String,
    token: String,
    queries: Vec<String>,
    exclude: Vec<String>,
    http: Http,
}

impl GitHub {
    pub fn new(acc: &GithubAccount) -> Result<Self> {
        Ok(GitHub {
            api_url: acc.api_url.trim_end_matches('/').to_string(),
            host: acc.host(),
            token: acc.resolve_token()?,
            queries: acc.queries.clone(),
            exclude: acc.exclude.clone(),
            http: Http::new(),
        })
    }

    fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        self.http.get_json(
            url,
            &[
                ("Authorization", format!("Bearer {}", self.token)),
                ("Accept", "application/vnd.github+json".to_string()),
                ("X-GitHub-Api-Version", "2022-11-28".to_string()),
            ],
        )
    }

    fn convert(&self, it: GhIssue) -> Option<RemoteItem> {
        // repository_url looks like https://api.github.com/repos/owner/repo
        let mut parts = it.repository_url.rsplit('/');
        let repo = parts.next()?.to_string();
        let owner = parts.next()?.to_string();
        let (kind, state) = match &it.pull_request {
            Some(pr) => (
                RemoteKind::PullRequest,
                if it.state == "open" {
                    RemoteState::Open
                } else if pr.merged_at.is_some() {
                    RemoteState::Merged
                } else {
                    RemoteState::Closed
                },
            ),
            None => (
                RemoteKind::Issue,
                if it.state == "open" {
                    RemoteState::Open
                } else {
                    RemoteState::Closed
                },
            ),
        };
        Some(RemoteItem {
            r: RemoteRef {
                provider: Provider::GitHub,
                host: self.host.clone(),
                owner,
                repo,
                number: it.number,
                kind,
                url: it.html_url,
                author: it.user.map(|u| u.login).unwrap_or_default(),
                state,
                remote_updated_at: parse_time(&it.updated_at),
                in_queries: true,
            },
            title: it.title,
        })
    }
}

#[derive(Deserialize)]
struct GhSearch {
    items: Vec<GhIssue>,
}

#[derive(Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    html_url: String,
    state: String,
    updated_at: String,
    repository_url: String,
    user: Option<GhUser>,
    pull_request: Option<GhPullRef>,
}

#[derive(Deserialize)]
struct GhUser {
    login: String,
}

#[derive(Deserialize)]
struct GhPullRef {
    merged_at: Option<String>,
}

impl Forge for GitHub {
    fn provider(&self) -> Provider {
        Provider::GitHub
    }

    fn host(&self) -> String {
        self.host.clone()
    }

    fn exclude(&self) -> &[String] {
        &self.exclude
    }

    fn fetch_open(&self) -> Result<Vec<RemoteItem>> {
        let mut out = Vec::new();
        for q in &self.queries {
            for page in 1..=MAX_PAGES {
                let url = format!(
                    "{}/search/issues?q={}&per_page=100&page={}",
                    self.api_url,
                    urlencode(q),
                    page
                );
                let res: GhSearch = self.get(&url)?;
                let n = res.items.len();
                out.extend(res.items.into_iter().filter_map(|i| self.convert(i)));
                if n < 100 {
                    break;
                }
            }
        }
        Ok(out)
    }

    fn fetch_one(&self, owner: &str, repo: &str, number: u64) -> Result<RemoteItem> {
        let url = format!("{}/repos/{owner}/{repo}/issues/{number}", self.api_url);
        let it: GhIssue = self.get(&url)?;
        self.convert(it).context("unexpected item shape")
    }
}

// ----- Forgejo / Gitea ------------------------------------------------------

pub struct Forgejo {
    base: String,
    host: String,
    token: String,
    queries: Vec<String>,
    exclude: Vec<String>,
    http: Http,
}

impl Forgejo {
    pub fn new(acc: &ForgejoAccount) -> Result<Self> {
        Ok(Forgejo {
            base: acc.url.trim_end_matches('/').to_string(),
            host: acc.host(),
            token: acc.resolve_token()?,
            queries: acc.queries.clone(),
            exclude: acc.exclude.clone(),
            http: Http::new(),
        })
    }

    fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        self.http.get_json(
            url,
            &[
                ("Authorization", format!("token {}", self.token)),
                ("Accept", "application/json".to_string()),
            ],
        )
    }

    fn convert(&self, it: FjIssue) -> RemoteItem {
        let (owner, repo) = match &it.repository {
            Some(r) => (r.owner.clone(), r.name.clone()),
            None => {
                // Fall back to the URL: https://host/owner/repo/(pulls|issues)/n
                let mut parts = it.html_url.rsplit('/');
                let _n = parts.next();
                let _kind = parts.next();
                let repo = parts.next().unwrap_or_default().to_string();
                let owner = parts.next().unwrap_or_default().to_string();
                (owner, repo)
            }
        };
        let (kind, state) = match &it.pull_request {
            Some(pr) => (
                RemoteKind::PullRequest,
                if it.state == "open" {
                    RemoteState::Open
                } else if pr.merged {
                    RemoteState::Merged
                } else {
                    RemoteState::Closed
                },
            ),
            None => (
                RemoteKind::Issue,
                if it.state == "open" {
                    RemoteState::Open
                } else {
                    RemoteState::Closed
                },
            ),
        };
        RemoteItem {
            r: RemoteRef {
                provider: Provider::Forgejo,
                host: self.host.clone(),
                owner,
                repo,
                number: it.number,
                kind,
                url: it.html_url,
                author: it.user.map(|u| u.login).unwrap_or_default(),
                state,
                remote_updated_at: parse_time(&it.updated_at),
                in_queries: true,
            },
            title: it.title,
        }
    }
}

#[derive(Deserialize)]
struct FjIssue {
    number: u64,
    title: String,
    html_url: String,
    state: String,
    updated_at: String,
    repository: Option<FjRepo>,
    user: Option<FjUser>,
    pull_request: Option<FjPullRef>,
}

#[derive(Deserialize)]
struct FjRepo {
    owner: String,
    name: String,
}

#[derive(Deserialize)]
struct FjUser {
    login: String,
}

#[derive(Deserialize)]
struct FjPullRef {
    #[serde(default)]
    merged: bool,
}

impl Forge for Forgejo {
    fn provider(&self) -> Provider {
        Provider::Forgejo
    }

    fn host(&self) -> String {
        self.host.clone()
    }

    fn exclude(&self) -> &[String] {
        &self.exclude
    }

    fn fetch_open(&self) -> Result<Vec<RemoteItem>> {
        let mut out = Vec::new();
        for q in &self.queries {
            for page in 1..=MAX_PAGES {
                let url = format!(
                    "{}/api/v1/repos/issues/search?state=open&limit=50&page={}&{}",
                    self.base, page, q
                );
                let res: Vec<FjIssue> = self.get(&url)?;
                let n = res.len();
                out.extend(res.into_iter().map(|i| self.convert(i)));
                // Servers may cap `limit` below 50, so only an empty page ends
                // the walk.
                if n == 0 {
                    break;
                }
            }
        }
        Ok(out)
    }

    fn fetch_one(&self, owner: &str, repo: &str, number: u64) -> Result<RemoteItem> {
        let url = format!("{}/api/v1/repos/{owner}/{repo}/issues/{number}", self.base);
        let it: FjIssue = self.get(&url)?;
        Ok(self.convert(it))
    }
}

/// A 404 or 410 from an individual fetch.
fn is_gone(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<ureq::Error>(),
        Some(ureq::Error::StatusCode(404 | 410))
    )
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ----- the loop -------------------------------------------------------------

/// Build one forge per configured account. Accounts that cannot be set up
/// (no token) are reported as notices and skipped.
pub fn build_forges(cfg: &Config, shared: &SharedRef) -> Vec<Box<dyn Forge>> {
    let mut forges: Vec<Box<dyn Forge>> = Vec::new();
    for acc in &cfg.github {
        match GitHub::new(acc) {
            Ok(f) => forges.push(Box::new(f)),
            Err(e) => shared
                .lock()
                .unwrap()
                .push_notice(Notice::error(format!("GitHub {}: {e:#}", acc.host()))),
        }
    }
    for acc in &cfg.forgejo {
        match Forgejo::new(acc) {
            Ok(f) => forges.push(Box::new(f)),
            Err(e) => shared
                .lock()
                .unwrap()
                .push_notice(Notice::error(format!("Forgejo {}: {e:#}", acc.host()))),
        }
    }
    forges
}

/// Runs until `Quit` arrives or the sender goes away.
pub fn run_loop(
    forges: Vec<Box<dyn Forge>>,
    shared: SharedRef,
    rx: Receiver<SyncCmd>,
    interval: Duration,
) {
    let mut round = 0usize;
    loop {
        sync_once(&forges, &shared, round);
        round = round.wrapping_add(1);
        match rx.recv_timeout(interval) {
            Ok(SyncCmd::Now) | Err(RecvTimeoutError::Timeout) => continue,
            Ok(SyncCmd::Quit) | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// `round` rotates which tracked items get an individual check when there
/// are more than `MAX_INDIVIDUAL_CHECKS` of them.
pub fn sync_once(forges: &[Box<dyn Forge>], shared: &SharedRef, round: usize) {
    if forges.is_empty() {
        return;
    }
    {
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        s.sync.in_progress = true;
        s.notify();
    }
    let mut errors = Vec::new();
    for forge in forges {
        if let Err(e) = sync_forge(forge.as_ref(), shared, round) {
            let msg = format!("{} {}: {e:#}", forge.provider().label(), forge.host());
            log::warn!("{msg}");
            errors.push(msg);
        }
    }
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

fn sync_forge(forge: &dyn Forge, shared: &SharedRef, round: usize) -> Result<()> {
    let mut items = forge.fetch_open()?;
    let excluded = |r: &RemoteRef| is_excluded(forge.exclude(), &r.owner, &r.repo);
    items.retain(|i| !excluded(&i.r));
    let seen: HashSet<String> = items.iter().map(|i| i.r.key()).collect();

    // Tracked open items that the queries no longer return: check each one
    // so we notice closes and merges.
    let mut to_check: Vec<(RemoteRef, String)> = {
        let s = shared.lock().unwrap_or_else(|e| e.into_inner());
        s.store
            .tasks()
            .iter()
            .filter_map(|t| t.remote().map(|r| (r.clone(), t.title.clone())))
            .filter(|(r, _)| {
                r.provider == forge.provider()
                    && r.host == forge.host()
                    && r.state == RemoteState::Open
                    && !seen.contains(&r.key())
                    && !excluded(r)
            })
            .collect()
    };
    if to_check.len() > MAX_INDIVIDUAL_CHECKS {
        let start = (round * MAX_INDIVIDUAL_CHECKS) % to_check.len();
        to_check.rotate_left(start);
        to_check.truncate(MAX_INDIVIDUAL_CHECKS);
    }
    for (r, title) in to_check {
        match forge.fetch_one(&r.owner, &r.repo, r.number) {
            Ok(mut item) => {
                item.r.in_queries = false;
                items.push(item);
            }
            Err(e) if is_gone(&e) => {
                // The item, repo, or access is gone. Treat it as closed so the
                // task can settle and be archived.
                log::info!("{} is gone on {}", r.label(), r.host);
                items.push(RemoteItem {
                    r: RemoteRef {
                        state: RemoteState::Closed,
                        in_queries: false,
                        ..r
                    },
                    title,
                });
            }
            Err(e) => log::warn!("check {}: {e:#}", r.label()),
        }
    }

    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    // Tasks from repositories that are now excluded go to the archive.
    let put_away = s.store.archive_matching(|t| {
        t.remote().is_some_and(|r| {
            r.provider == forge.provider() && r.host == forge.host() && excluded(r)
        })
    });
    let excluded_any = !put_away.is_empty();
    for label in put_away {
        s.push_notice(Notice::info(format!("Excluded repository: {label}")));
    }
    let events = s.store.apply_remote(&items);
    if (!events.is_empty() || excluded_any)
        && let Err(e) = s.store.save()
    {
        log::error!("save store: {e:#}");
    }
    for ev in events {
        let notice = match ev {
            SyncEvent::New { label, .. } => Notice::info(format!("New: {label}")),
            SyncEvent::Closed {
                label,
                merged: true,
                ..
            } => Notice::info(format!("Merged: {label}")),
            SyncEvent::Closed {
                label,
                merged: false,
                ..
            } => Notice::info(format!("Closed: {label}")),
            SyncEvent::ReopenedRemotely { label, .. } => Notice::info(format!("Reopened: {label}")),
            SyncEvent::Woken { label, .. } => Notice::info(format!("Activity: {label}")),
            SyncEvent::Released { label, .. } => {
                Notice::info(format!("Not waiting on you: {label}"))
            }
            SyncEvent::Requested { label, .. } => Notice::info(format!("Needs you: {label}")),
        };
        s.push_notice(notice);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_keeps_search_syntax_readable() {
        assert_eq!(
            urlencode("is:open is:pr author:@me"),
            "is%3Aopen%20is%3Apr%20author%3A%40me"
        );
    }

    #[test]
    fn github_search_item_converts() {
        let gh = GitHub {
            api_url: "https://api.github.com".into(),
            host: "github.com".into(),
            token: String::new(),
            queries: vec![],
            exclude: vec![],
            http: Http::new(),
        };
        let json = r#"{"number": 7, "title": "Fix it", "html_url": "https://github.com/o/r/pull/7",
            "state": "closed", "updated_at": "2026-02-03T04:05:06Z",
            "repository_url": "https://api.github.com/repos/o/r",
            "user": {"login": "ben"}, "pull_request": {"merged_at": "2026-02-03T04:05:06Z"}}"#;
        let it: GhIssue = serde_json::from_str(json).unwrap();
        let item = gh.convert(it).unwrap();
        assert_eq!(item.r.owner, "o");
        assert_eq!(item.r.repo, "r");
        assert_eq!(item.r.kind, RemoteKind::PullRequest);
        assert_eq!(item.r.state, RemoteState::Merged);
        assert_eq!(item.r.key(), "github:github.com/o/r#7");
    }

    #[test]
    fn forgejo_item_converts() {
        let fj = Forgejo {
            base: "https://codeberg.org".into(),
            host: "codeberg.org".into(),
            token: String::new(),
            queries: vec![],
            exclude: vec![],
            http: Http::new(),
        };
        let json = r#"{"number": 3, "title": "Bug", "html_url": "https://codeberg.org/o/r/issues/3",
            "state": "open", "updated_at": "2026-02-03T04:05:06Z",
            "repository": {"owner": "o", "name": "r", "full_name": "o/r"},
            "user": {"login": "ben"}, "pull_request": null}"#;
        let it: FjIssue = serde_json::from_str(json).unwrap();
        let item = fj.convert(it);
        assert_eq!(item.r.kind, RemoteKind::Issue);
        assert_eq!(item.r.state, RemoteState::Open);
        assert_eq!(item.r.project(), "o/r");
    }
}
