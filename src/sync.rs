//! Remote providers (GitHub, Forgejo) and the background sync loop.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::config::{Config, ForgejoAccount, GithubAccount, is_excluded};
use crate::model::{
    Checks, MergeState, OnClose, Provider, RemoteItem, RemoteKind, RemoteRef, RemoteState,
    SyncEvent,
};
use crate::shared::{Notice, SharedRef};

const USER_AGENT: &str = concat!("lunch-tray/", env!("CARGO_PKG_VERSION"));
const MAX_PAGES: usize = 5;
const MAX_INDIVIDUAL_CHECKS: usize = 60;
/// Pull requests per GraphQL request. GitHub gives a query about ten
/// seconds; bigger batches across many repositories time out with a 502.
const GRAPHQL_BATCH: usize = 15;
/// Forgejo needs two calls per pull request; cap each poll.
const FORGEJO_CHECKS_PER_POLL: usize = 20;

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
    /// True when the host's quota is nearly spent; optional work waits.
    fn quota_low(&self) -> bool {
        false
    }
    /// CI and merge state for these open pull requests. Items the forge
    /// cannot answer for are simply left out.
    fn fetch_checks(&self, prs: &[RemoteRef]) -> Result<Vec<(String, Checks)>>;
}

/// Sort CI contexts into passed, failed, or pending buckets. Skipped jobs
/// never ran, so they count for nothing.
fn tally(states: impl IntoIterator<Item = CheckOutcome>) -> (u32, u32, u32) {
    let (mut passed, mut failed, mut pending) = (0, 0, 0);
    for s in states {
        match s {
            CheckOutcome::Passed => passed += 1,
            CheckOutcome::Failed => failed += 1,
            CheckOutcome::Pending => pending += 1,
            CheckOutcome::Skipped => {}
        }
    }
    (passed, failed, pending)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckOutcome {
    Passed,
    Failed,
    Pending,
    Skipped,
}

/// One HTTP client per account. Error statuses come back as responses so
/// the proof-of-work gate can be read before deciding what failed.
struct Http {
    agent: ureq::Agent,
    /// Cookie earned from a proof-of-work gate, if the host has one.
    gate_cookie: Mutex<Option<String>>,
    /// ETag and body of the last answer per GET URL. A matching `304`
    /// costs nothing against GitHub's rate limit.
    etags: Mutex<HashMap<String, (String, String)>>,
    /// GitHub's own count per quota, read from the latest answer's headers.
    quotas: Mutex<HashMap<String, Quota>>,
    /// Set when the host said to stop; nothing is sent before it.
    paused_until: Mutex<Option<DateTime<Utc>>>,
}

#[derive(Clone, Copy, Debug)]
struct Quota {
    limit: u64,
    remaining: u64,
    reset: DateTime<Utc>,
}

/// GitHub's rate-limit headers, when present.
fn parse_quota(headers: &ureq::http::HeaderMap) -> Option<(String, Quota)> {
    let get = |k: &str| headers.get(k).and_then(|v| v.to_str().ok());
    let resource = get("x-ratelimit-resource").unwrap_or("core").to_string();
    let limit: u64 = get("x-ratelimit-limit")?.parse().ok()?;
    let remaining: u64 = get("x-ratelimit-remaining")?.parse().ok()?;
    let reset: i64 = get("x-ratelimit-reset")?.parse().ok()?;
    let reset = DateTime::from_timestamp(reset, 0)?;
    Some((
        resource,
        Quota {
            limit,
            remaining,
            reset,
        },
    ))
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
            etags: Mutex::new(HashMap::new()),
            quotas: Mutex::new(HashMap::new()),
            paused_until: Mutex::new(None),
        }
    }

    /// True while the host asked us to back off.
    fn paused(&self) -> Option<DateTime<Utc>> {
        let mut p = self.paused_until.lock().unwrap_or_else(|e| e.into_inner());
        match *p {
            Some(until) if until > Utc::now() => Some(until),
            Some(_) => {
                *p = None;
                None
            }
            None => None,
        }
    }

    fn pause_until(&self, until: DateTime<Utc>) {
        *self.paused_until.lock().unwrap_or_else(|e| e.into_inner()) = Some(until);
    }

    /// True when any quota is in its last tenth. Background work waits
    /// for the reset so the searches that keep the list current still fit.
    fn quota_low(&self) -> bool {
        let now = Utc::now();
        self.quotas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|q| q.reset > now && q.remaining * 10 < q.limit)
    }

    /// GET `url` with `headers` and decode JSON. A 403 carrying a
    /// proof-of-work challenge page is solved once, then the request is
    /// retried with the earned cookie.
    fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        headers: &[(&str, String)],
    ) -> Result<T> {
        self.request_json(url, headers, None)
    }

    /// POST a JSON body and decode the JSON reply.
    fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        headers: &[(&str, String)],
        body: &serde_json::Value,
    ) -> Result<T> {
        self.request_json(url, headers, Some(body))
    }

    fn request_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        headers: &[(&str, String)],
        body: Option<&serde_json::Value>,
    ) -> Result<T> {
        if let Some(until) = self.paused() {
            bail!(
                "rate limited until {}",
                until.with_timezone(&chrono::Local).format("%H:%M")
            );
        }
        let method = if body.is_some() { "POST" } else { "GET" };
        for attempt in 0..2 {
            let cookie = self
                .gate_cookie
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let cached = if body.is_none() {
                self.etags
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(url)
                    .cloned()
            } else {
                None
            };
            let mut resp = match body {
                None => {
                    let mut req = self.agent.get(url);
                    for (k, v) in headers {
                        req = req.header(*k, v);
                    }
                    if let Some(c) = &cookie {
                        req = req.header("Cookie", c);
                    }
                    if let Some((etag, _)) = &cached {
                        req = req.header("If-None-Match", etag);
                    }
                    req.call().with_context(|| format!("GET {url}"))?
                }
                Some(body) => {
                    let mut req = self.agent.post(url);
                    for (k, v) in headers {
                        req = req.header(*k, v);
                    }
                    if let Some(c) = &cookie {
                        req = req.header("Cookie", c);
                    }
                    req.send_json(body).with_context(|| format!("POST {url}"))?
                }
            };
            let status = resp.status().as_u16();
            if let Some((resource, quota)) = parse_quota(resp.headers()) {
                log::trace!("{resource} quota: {}/{}", quota.remaining, quota.limit);
                self.quotas
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(resource, quota);
            }
            if status == 304
                && let Some((_, text)) = cached
            {
                log::debug!("unchanged: {url}");
                return serde_json::from_str::<T>(&text).with_context(|| format!("decode {url}"));
            }
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
            if status == 403 || status == 429 {
                // GitHub says 403 for its primary limit and 403 or 429 for
                // the secondary one, with a Retry-After. Stop until then.
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<i64>().ok());
                let exhausted = parse_quota(resp.headers())
                    .filter(|(_, q)| q.remaining == 0)
                    .map(|(_, q)| q.reset);
                let message = resp
                    .body_mut()
                    .read_to_string()
                    .ok()
                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                    .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
                    .unwrap_or_default();
                let until = match (retry_after, exhausted) {
                    (Some(secs), _) => Some(Utc::now() + chrono::Duration::seconds(secs.max(1))),
                    (None, Some(reset)) => Some(reset),
                    (None, None) if message.to_lowercase().contains("rate limit") => {
                        Some(Utc::now() + chrono::Duration::seconds(60))
                    }
                    _ => None,
                };
                if let Some(until) = until {
                    log::warn!("{method} {url}: {status}, backing off until {until}: {message}");
                    self.pause_until(until);
                    bail!(
                        "rate limited until {}",
                        until.with_timezone(&chrono::Local).format("%H:%M")
                    );
                }
                return Err(ureq::Error::StatusCode(status))
                    .with_context(|| format!("{method} {url}: {message}"));
            }
            if status >= 400 {
                return Err(ureq::Error::StatusCode(status))
                    .with_context(|| format!("{method} {url}"));
            }
            let etag = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let text = resp
                .body_mut()
                .read_to_string()
                .with_context(|| format!("read {url}"))?;
            if body.is_none()
                && let Some(etag) = etag
            {
                self.etags
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(url.to_string(), (etag, text.clone()));
            }
            return serde_json::from_str::<T>(&text).with_context(|| format!("decode {url}"));
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
    /// The GraphQL endpoint next to the REST one.
    fn graphql_url(&self) -> String {
        match self.api_url.strip_suffix("/api/v3") {
            Some(base) => format!("{base}/api/graphql"),
            None => format!("{}/graphql", self.api_url),
        }
    }

    fn checks_batch(&self, batch: &[RemoteRef]) -> Result<Vec<(String, Checks)>> {
        let mut query = String::from("query {");
        for (i, r) in batch.iter().enumerate() {
            query.push_str(&format!(
                " p{i}: repository(owner: {owner}, name: {repo}) {{ pullRequest(number: {n}) {{ ...pr }} }}",
                owner = serde_json::to_string(&r.owner)?,
                repo = serde_json::to_string(&r.repo)?,
                n = r.number
            ));
        }
        query.push_str(
            " } fragment pr on PullRequest { headRefOid mergeable mergeStateStatus \
             commits(last: 1) { nodes { commit { statusCheckRollup { state \
             contexts(first: 100) { nodes { __typename \
             ... on CheckRun { name status conclusion startedAt \
             checkSuite { workflowRun { workflow { name } } } } \
             ... on StatusContext { context state createdAt } } } } } } } }",
        );
        let reply: GqlReply = self.http.post_json(
            &self.graphql_url(),
            &[("Authorization", format!("Bearer {}", self.token))],
            &serde_json::json!({ "query": query }),
        )?;
        if let Some(errors) = &reply.errors {
            for e in errors {
                log::debug!("graphql: {}", e.message);
            }
        }
        let mut out = Vec::new();
        let Some(data) = reply.data else {
            bail!("graphql reply carried no data");
        };
        let now = Utc::now();
        for (i, r) in batch.iter().enumerate() {
            let node = data
                .get(format!("p{i}"))
                .and_then(|v| v.get("pullRequest"))
                .cloned()
                .filter(|v| !v.is_null());
            let Some(node) = node else {
                continue;
            };
            match serde_json::from_value::<GqlPr>(node) {
                Ok(pr) => out.push((r.key(), pr.into_checks(now))),
                Err(e) => log::warn!("checks for {}: {e}", r.label()),
            }
        }
        Ok(out)
    }

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
                draft: it.draft,
                remote_updated_at: parse_time(&it.updated_at),
                in_queries: true,
                checks: None,
            },
            title: it.title,
        })
    }
}

impl GitHub {
    fn convert_node(&self, node: GqlSearchNode) -> Option<RemoteItem> {
        let (kind, state, draft, c) = match node {
            GqlSearchNode::PullRequest { is_draft, common } => {
                let state = match common.state.as_str() {
                    "OPEN" => RemoteState::Open,
                    "MERGED" => RemoteState::Merged,
                    _ => RemoteState::Closed,
                };
                (RemoteKind::PullRequest, state, is_draft, common)
            }
            GqlSearchNode::Issue { common } => {
                let state = if common.state == "OPEN" {
                    RemoteState::Open
                } else {
                    RemoteState::Closed
                };
                (RemoteKind::Issue, state, false, common)
            }
            GqlSearchNode::Other => return None,
        };
        let (owner, repo) = c.repository.name_with_owner.split_once('/')?;
        Some(RemoteItem {
            r: RemoteRef {
                provider: Provider::GitHub,
                host: self.host.clone(),
                owner: owner.to_string(),
                repo: repo.to_string(),
                number: c.number,
                kind,
                url: c.url,
                author: c.author.map(|a| a.login).unwrap_or_default(),
                state,
                draft,
                remote_updated_at: parse_time(&c.updated_at),
                in_queries: true,
                checks: None,
            },
            title: c.title,
        })
    }
}

#[derive(Deserialize)]
struct GqlSearchPage {
    #[serde(rename = "pageInfo")]
    page_info: GqlPageInfo,
    nodes: Vec<GqlSearchNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum GqlSearchNode {
    PullRequest {
        #[serde(rename = "isDraft")]
        is_draft: bool,
        #[serde(flatten)]
        common: GqlSearchCommon,
    },
    Issue {
        #[serde(flatten)]
        common: GqlSearchCommon,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlSearchCommon {
    number: u64,
    title: String,
    url: String,
    state: String,
    updated_at: String,
    author: Option<GqlLogin>,
    repository: GqlRepoName,
}

#[derive(Deserialize)]
struct GqlLogin {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlRepoName {
    name_with_owner: String,
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
    #[serde(default)]
    draft: bool,
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

    fn quota_low(&self) -> bool {
        self.http.quota_low()
    }

    /// Batches of `GRAPHQL_BATCH` pull requests per GraphQL request: each
    /// alias asks for the merge state and the check rollup of the head
    /// commit. A batch that fails is split in half and retried, down to
    /// single pull requests, so one slow repository cannot stall the rest.
    fn fetch_checks(&self, prs: &[RemoteRef]) -> Result<Vec<(String, Checks)>> {
        let mut out = Vec::new();
        let mut queue: std::collections::VecDeque<&[RemoteRef]> =
            prs.chunks(GRAPHQL_BATCH).collect();
        while let Some(batch) = queue.pop_front() {
            match self.checks_batch(batch) {
                Ok(found) => out.extend(found),
                Err(e) if is_auth_error(&e) => return Err(e),
                Err(e) if batch.len() > 1 => {
                    log::debug!("checks batch of {} failed, splitting: {e:#}", batch.len());
                    let (a, b) = batch.split_at(batch.len() / 2);
                    queue.push_front(b);
                    queue.push_front(a);
                }
                Err(e) => log::warn!("checks for {}: {e:#}", batch[0].label()),
            }
        }
        Ok(out)
    }

    /// The configured searches through GraphQL: one point of the hourly
    /// budget per page, rather than one of the 30-a-minute REST search
    /// quota. The query syntax is the same.
    fn fetch_open(&self) -> Result<Vec<RemoteItem>> {
        const QUERY: &str = "query($q: String!, $after: String) { \
            search(query: $q, type: ISSUE, first: 100, after: $after) { \
            pageInfo { hasNextPage endCursor } nodes { __typename \
            ... on PullRequest { number title url state updatedAt isDraft \
            author { login } repository { nameWithOwner } } \
            ... on Issue { number title url state updatedAt \
            author { login } repository { nameWithOwner } } } } }";
        let mut out = Vec::new();
        for q in &self.queries {
            let mut after: Option<String> = None;
            for _ in 0..MAX_PAGES {
                let reply: GqlReply = self.http.post_json(
                    &self.graphql_url(),
                    &[("Authorization", format!("Bearer {}", self.token))],
                    &serde_json::json!({ "query": QUERY, "variables": { "q": q, "after": after } }),
                )?;
                let page: GqlSearchPage = match reply.data.and_then(|d| d.get("search").cloned()) {
                    Some(v) if !v.is_null() => {
                        serde_json::from_value(v).context("decode search")?
                    }
                    _ => {
                        let why = reply
                            .errors
                            .and_then(|e| e.into_iter().next())
                            .map(|e| e.message)
                            .unwrap_or_else(|| "no data".into());
                        bail!("search {q:?}: {why}");
                    }
                };
                out.extend(page.nodes.into_iter().filter_map(|n| self.convert_node(n)));
                if page.page_info.has_next_page && page.page_info.end_cursor.is_some() {
                    after = page.page_info.end_cursor;
                } else {
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

#[derive(Deserialize)]
struct GqlReply {
    data: Option<serde_json::Value>,
    errors: Option<Vec<GqlError>>,
}

#[derive(Deserialize)]
struct GqlError {
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPr {
    head_ref_oid: String,
    mergeable: String,
    merge_state_status: Option<String>,
    commits: GqlCommits,
}

#[derive(Deserialize)]
struct GqlCommits {
    nodes: Vec<GqlCommitNode>,
}

#[derive(Deserialize)]
struct GqlCommitNode {
    commit: GqlCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlCommit {
    status_check_rollup: Option<GqlRollup>,
}

#[derive(Deserialize)]
struct GqlRollup {
    contexts: GqlContexts,
}

#[derive(Deserialize)]
struct GqlContexts {
    nodes: Vec<GqlContext>,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum GqlContext {
    #[serde(rename_all = "camelCase")]
    CheckRun {
        name: String,
        status: String,
        conclusion: Option<String>,
        started_at: Option<String>,
        check_suite: Option<GqlCheckSuite>,
    },
    #[serde(rename_all = "camelCase")]
    StatusContext {
        context: String,
        state: String,
        created_at: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlCheckSuite {
    workflow_run: Option<GqlWorkflowRun>,
}

#[derive(Deserialize)]
struct GqlWorkflowRun {
    workflow: GqlWorkflow,
}

#[derive(Deserialize)]
struct GqlWorkflow {
    name: String,
}

/// Re-running a workflow leaves the earlier runs on the commit next to the
/// new ones. Keep only the latest run of each job, as GitHub's own view
/// does. Jobs are told apart by workflow and name, since two workflows
/// can both have a job called `build`.
fn latest_per_name(nodes: &[GqlContext]) -> Vec<&GqlContext> {
    let mut latest: std::collections::BTreeMap<String, (&str, &GqlContext)> =
        std::collections::BTreeMap::new();
    for node in nodes {
        let (name, when) = match node {
            GqlContext::CheckRun {
                name,
                started_at,
                check_suite,
                ..
            } => {
                let workflow = check_suite
                    .as_ref()
                    .and_then(|c| c.workflow_run.as_ref())
                    .map(|w| w.workflow.name.as_str())
                    .unwrap_or("");
                (
                    format!("{workflow} / {name}"),
                    started_at.as_deref().unwrap_or(""),
                )
            }
            GqlContext::StatusContext {
                context,
                created_at,
                ..
            } => (context.clone(), created_at.as_deref().unwrap_or("")),
            GqlContext::Other => continue,
        };
        // ISO 8601 timestamps sort as text; ties go to the later entry.
        match latest.get(&name) {
            Some((seen, _)) if *seen > when => {}
            _ => {
                latest.insert(name, (when, node));
            }
        }
    }
    latest.into_values().map(|(_, node)| node).collect()
}

impl GqlContext {
    fn outcome(&self) -> CheckOutcome {
        match self {
            GqlContext::CheckRun {
                status, conclusion, ..
            } => {
                if status != "COMPLETED" {
                    return CheckOutcome::Pending;
                }
                match conclusion.as_deref() {
                    Some("SUCCESS" | "NEUTRAL") => CheckOutcome::Passed,
                    // Cut short by a failing sibling or a newer run: not a
                    // result of its own.
                    Some("SKIPPED" | "CANCELLED" | "STALE") => CheckOutcome::Skipped,
                    None => CheckOutcome::Pending,
                    Some(_) => CheckOutcome::Failed,
                }
            }
            GqlContext::StatusContext { state, .. } => match state.as_str() {
                "SUCCESS" => CheckOutcome::Passed,
                "PENDING" | "EXPECTED" => CheckOutcome::Pending,
                _ => CheckOutcome::Failed,
            },
            GqlContext::Other => CheckOutcome::Passed,
        }
    }
}

impl GqlPr {
    fn into_checks(self, now: chrono::DateTime<Utc>) -> Checks {
        let merge = match (
            self.mergeable.as_str(),
            self.merge_state_status.as_deref().unwrap_or("UNKNOWN"),
        ) {
            ("CONFLICTING", _) | (_, "DIRTY") => MergeState::Conflicting,
            ("UNKNOWN", _) | (_, "UNKNOWN") => MergeState::Unknown,
            (_, "BEHIND") => MergeState::Behind,
            (_, "BLOCKED") => MergeState::Blocked,
            (_, "UNSTABLE") => MergeState::Unstable,
            _ => MergeState::Clean,
        };
        let rollup = self
            .commits
            .nodes
            .into_iter()
            .next()
            .and_then(|n| n.commit.status_check_rollup);
        let (passed, failed, pending, total) = match rollup {
            Some(r) => {
                let runs = latest_per_name(&r.contexts.nodes);
                let (p, f, pe) = tally(runs.iter().map(|c| c.outcome()));
                (p, f, pe, p + f + pe)
            }
            None => (0, 0, 0, 0),
        };
        Checks {
            passed,
            failed,
            pending,
            total,
            merge,
            head_sha: self.head_ref_oid,
            checked_at: now,
        }
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
        let draft = it.pull_request.as_ref().is_some_and(|pr| pr.draft);
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
                draft,
                remote_updated_at: parse_time(&it.updated_at),
                in_queries: true,
                checks: None,
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
struct FjPull {
    mergeable: Option<bool>,
    head: FjHead,
}

#[derive(Deserialize)]
struct FjHead {
    sha: String,
}

#[derive(Deserialize)]
struct FjCombinedStatus {
    /// Forgejo sends `null` rather than an empty list when there are none.
    #[serde(default)]
    statuses: Option<Vec<FjStatus>>,
}

#[derive(Deserialize)]
struct FjStatus {
    status: String,
}

#[derive(Deserialize)]
struct FjPullRef {
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    draft: bool,
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

    /// Two calls per pull request: the PR for its head and mergeability,
    /// then the combined status of that commit.
    fn fetch_checks(&self, prs: &[RemoteRef]) -> Result<Vec<(String, Checks)>> {
        let mut out = Vec::new();
        for r in prs.iter().take(FORGEJO_CHECKS_PER_POLL) {
            let pr_url = format!(
                "{}/api/v1/repos/{}/{}/pulls/{}",
                self.base, r.owner, r.repo, r.number
            );
            let pr: FjPull = match self.get(&pr_url) {
                Ok(pr) => pr,
                Err(e) => {
                    log::warn!("checks for {}: {e:#}", r.label());
                    continue;
                }
            };
            let status_url = format!(
                "{}/api/v1/repos/{}/{}/commits/{}/status",
                self.base, r.owner, r.repo, pr.head.sha
            );
            let combined: FjCombinedStatus = match self.get(&status_url) {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("status for {}: {e:#}", r.label());
                    continue;
                }
            };
            let statuses = combined.statuses.unwrap_or_default();
            let (passed, failed, pending) =
                tally(statuses.iter().map(|st| match st.status.as_str() {
                    // Forgejo states: success, warning, pending, skipped,
                    // failure, error.
                    "success" | "warning" => CheckOutcome::Passed,
                    "pending" => CheckOutcome::Pending,
                    "skipped" => CheckOutcome::Skipped,
                    _ => CheckOutcome::Failed,
                }));
            let merge = match pr.mergeable {
                Some(true) => MergeState::Clean,
                Some(false) => MergeState::Conflicting,
                None => MergeState::Unknown,
            };
            out.push((
                r.key(),
                Checks {
                    passed,
                    failed,
                    pending,
                    total: passed + failed + pending,
                    merge,
                    head_sha: pr.head.sha,
                    checked_at: Utc::now(),
                },
            ));
        }
        Ok(out)
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

/// Second phase: CI and merge state for the open pull requests that are
/// due, see `Store::checks_due`.
fn sync_checks(forge: &dyn Forge, shared: &SharedRef) -> Result<()> {
    let due = {
        let s = shared.lock().unwrap_or_else(|e| e.into_inner());
        s.store
            .checks_due(forge.provider(), &forge.host(), Utc::now())
    };
    if due.is_empty() {
        return Ok(());
    }
    log::debug!("checking CI state of {} pull requests", due.len());
    let results = forge.fetch_checks(&due)?;
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    let mut changed = false;
    for (key, checks) in results {
        changed |= s.store.set_checks(&key, checks);
    }
    // Always save: checked_at moved even when the numbers did not.
    if let Err(e) = s.store.save() {
        log::error!("save store: {e:#}");
    }
    if changed {
        s.notify();
    }
    Ok(())
}

/// A 401 or 403: the token is the problem, not the request.
fn is_auth_error(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<ureq::Error>(),
        Some(ureq::Error::StatusCode(401 | 403))
    )
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
    on_close: OnClose,
    checks: bool,
) {
    let mut round = 0usize;
    loop {
        sync_once(&forges, &shared, on_close, checks, round);
        round = round.wrapping_add(1);
        match rx.recv_timeout(interval) {
            Ok(SyncCmd::Now) | Err(RecvTimeoutError::Timeout) => continue,
            Ok(SyncCmd::Quit) | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// `round` rotates which tracked items get an individual check when there
/// are more than `MAX_INDIVIDUAL_CHECKS` of them.
pub fn sync_once(
    forges: &[Box<dyn Forge>],
    shared: &SharedRef,
    on_close: OnClose,
    checks: bool,
    round: usize,
) {
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
        if let Err(e) = sync_forge(forge.as_ref(), shared, on_close, round) {
            let msg = format!("{} {}: {e:#}", forge.provider().label(), forge.host());
            log::warn!("{msg}");
            errors.push(msg);
        }
        if checks && forge.quota_low() {
            log::info!(
                "{} quota is low, skipping CI checks this poll",
                forge.host()
            );
        } else if checks && let Err(e) = sync_checks(forge.as_ref(), shared) {
            log::warn!(
                "{} {} checks: {e:#}",
                forge.provider().label(),
                forge.host()
            );
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

fn sync_forge(
    forge: &dyn Forge,
    shared: &SharedRef,
    on_close: OnClose,
    round: usize,
) -> Result<()> {
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
    if !to_check.is_empty() && forge.quota_low() {
        log::info!(
            "{} quota is low, skipping {} individual checks",
            forge.host(),
            to_check.len()
        );
        to_check.clear();
    }
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
    let events = s.store.apply_remote(on_close, &items);
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
            "user": {"login": "ben"}, "pull_request": {"merged_at": "2026-02-03T04:05:06Z"},
            "draft": true}"#;
        let it: GhIssue = serde_json::from_str(json).unwrap();
        let item = gh.convert(it).unwrap();
        assert!(item.r.draft);
        assert_eq!(item.r.owner, "o");
        assert_eq!(item.r.repo, "r");
        assert_eq!(item.r.kind, RemoteKind::PullRequest);
        assert_eq!(item.r.state, RemoteState::Merged);
        assert_eq!(item.r.key(), "github:github.com/o/r#7");
    }

    #[test]
    fn graphql_search_nodes_convert() {
        let gh = GitHub {
            api_url: "https://api.github.com".into(),
            host: "github.com".into(),
            token: String::new(),
            queries: vec![],
            exclude: vec![],
            http: Http::new(),
        };
        let json = r#"{"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": [
            {"__typename": "PullRequest", "number": 7, "title": "Fix it", "url": "https://github.com/o/r/pull/7",
             "state": "MERGED", "updatedAt": "2026-02-03T04:05:06Z", "isDraft": true,
             "author": {"login": "ben"}, "repository": {"nameWithOwner": "o/r"}},
            {"__typename": "Issue", "number": 9, "title": "Bug", "url": "https://github.com/o/r/issues/9",
             "state": "OPEN", "updatedAt": "2026-02-03T04:05:06Z",
             "author": null, "repository": {"nameWithOwner": "o/r"}},
            {"__typename": "Discussion"}]}"#;
        let page: GqlSearchPage = serde_json::from_str(json).unwrap();
        let items: Vec<RemoteItem> = page
            .nodes
            .into_iter()
            .filter_map(|n| gh.convert_node(n))
            .collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].r.kind, RemoteKind::PullRequest);
        assert_eq!(items[0].r.state, RemoteState::Merged);
        assert!(items[0].r.draft);
        assert_eq!(items[0].r.key(), "github:github.com/o/r#7");
        assert_eq!(items[1].r.kind, RemoteKind::Issue);
        assert_eq!(items[1].r.author, "");
        assert!(items[1].r.in_queries);
    }

    #[test]
    fn graphql_reply_becomes_checks() {
        let json = r#"{"headRefOid": "abc123", "mergeable": "MERGEABLE", "mergeStateStatus": "BLOCKED",
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "PENDING",
            "contexts": {"totalCount": 4, "nodes": [
                {"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "startedAt": "2026-09-29T10:00:00Z"},
                {"__typename": "CheckRun", "name": "lint", "status": "COMPLETED", "conclusion": "FAILURE", "startedAt": "2026-09-29T10:00:00Z"},
                {"__typename": "CheckRun", "name": "test", "status": "IN_PROGRESS", "conclusion": null, "startedAt": "2026-09-29T10:00:00Z"},
                {"__typename": "StatusContext", "context": "ci/external", "state": "SUCCESS", "createdAt": "2026-09-29T10:00:00Z"}]}}}}]}}"#;
        let pr: GqlPr = serde_json::from_str(json).unwrap();
        let c = pr.into_checks(Utc::now());
        assert_eq!((c.passed, c.failed, c.pending, c.total), (2, 1, 1, 4));
        assert_eq!(c.merge, MergeState::Blocked);
        assert_eq!(c.head_sha, "abc123");
        assert!(c.unsettled());

        let json = r#"{"headRefOid": "def", "mergeable": "CONFLICTING", "mergeStateStatus": "DIRTY",
            "commits": {"nodes": [{"commit": {"statusCheckRollup": null}}]}}"#;
        let c: Checks = serde_json::from_str::<GqlPr>(json)
            .unwrap()
            .into_checks(Utc::now());
        assert_eq!(c.merge, MergeState::Conflicting);
        assert_eq!(c.total, 0);
        assert!(!c.unsettled());
    }

    #[test]
    fn rerun_checks_count_once_with_the_latest_result() {
        let json = r#"{"headRefOid": "abc", "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "SUCCESS",
            "contexts": {"totalCount": 5, "nodes": [
                {"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "FAILURE", "startedAt": "2026-09-29T09:00:00Z"},
                {"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "startedAt": "2026-09-29T10:00:00Z"},
                {"__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "SUCCESS", "startedAt": "2026-09-29T09:00:00Z"},
                {"__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "SUCCESS", "startedAt": "2026-09-29T10:00:00Z"},
                {"__typename": "CheckRun", "name": "build-and-test", "status": "COMPLETED", "conclusion": "SUCCESS", "startedAt": "2026-09-29T10:00:00Z",
                 "checkSuite": {"workflowRun": {"workflow": {"name": "PostgreSQL Tests"}}}},
                {"__typename": "CheckRun", "name": "build-and-test", "status": "COMPLETED", "conclusion": "CANCELLED", "startedAt": "2026-09-29T10:00:00Z",
                 "checkSuite": {"workflowRun": {"workflow": {"name": "0FC Tests"}}}},
                {"__typename": "StatusContext", "context": "ci/external", "state": "SUCCESS", "createdAt": "2026-09-29T09:00:00Z"}]}}}}]}}"#;
        let c = serde_json::from_str::<GqlPr>(json)
            .unwrap()
            .into_checks(Utc::now());
        // Two workflows share a job name and stay apart; the cancelled one
        // counts for nothing.
        assert_eq!((c.passed, c.failed, c.pending, c.total), (4, 0, 0, 4));
    }

    #[test]
    fn skipped_jobs_count_for_nothing() {
        let (p, f, pe) = tally([
            CheckOutcome::Passed,
            CheckOutcome::Skipped,
            CheckOutcome::Failed,
            CheckOutcome::Skipped,
            CheckOutcome::Pending,
        ]);
        assert_eq!((p, f, pe), (1, 1, 1));
        let json = r#"{"headRefOid": "abc", "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "SUCCESS",
            "contexts": {"nodes": [
                {"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "startedAt": "2026-10-06T10:00:00Z"},
                {"__typename": "CheckRun", "name": "notify-failure", "status": "COMPLETED", "conclusion": "SKIPPED", "startedAt": "2026-10-06T10:00:00Z"}]}}}}]}}"#;
        let c = serde_json::from_str::<GqlPr>(json)
            .unwrap()
            .into_checks(Utc::now());
        assert_eq!((c.passed, c.failed, c.pending, c.total), (1, 0, 0, 1));
    }

    #[test]
    fn rate_limit_headers_are_read() {
        let mut h = ureq::http::HeaderMap::new();
        h.insert("x-ratelimit-resource", "search".parse().unwrap());
        h.insert("x-ratelimit-limit", "30".parse().unwrap());
        h.insert("x-ratelimit-remaining", "2".parse().unwrap());
        h.insert("x-ratelimit-reset", "1800000000".parse().unwrap());
        let (resource, q) = parse_quota(&h).unwrap();
        assert_eq!(resource, "search");
        assert_eq!((q.limit, q.remaining), (30, 2));
        assert_eq!(q.reset.timestamp(), 1_800_000_000);
        assert!(parse_quota(&ureq::http::HeaderMap::new()).is_none());

        let http = Http::new();
        assert!(!http.quota_low());
        http.quotas.lock().unwrap().insert(
            "core".into(),
            Quota {
                limit: 5000,
                remaining: 400,
                reset: Utc::now() + chrono::Duration::minutes(10),
            },
        );
        assert!(http.quota_low());
        assert!(http.paused().is_none());
        http.pause_until(Utc::now() + chrono::Duration::minutes(1));
        assert!(http.paused().is_some());
    }

    #[test]
    fn graphql_url_sits_next_to_the_rest_api() {
        let mut gh = GitHub {
            api_url: "https://api.github.com".into(),
            host: "github.com".into(),
            token: String::new(),
            queries: vec![],
            exclude: vec![],
            http: Http::new(),
        };
        assert_eq!(gh.graphql_url(), "https://api.github.com/graphql");
        gh.api_url = "https://ghe.example.com/api/v3".into();
        assert_eq!(gh.graphql_url(), "https://ghe.example.com/api/graphql");
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
        assert!(!item.r.draft);
        assert_eq!(item.r.kind, RemoteKind::Issue);
        assert_eq!(item.r.state, RemoteState::Open);
        assert_eq!(item.r.project(), "o/r");
    }
}
