//! The GitHub sync: one pass over the configured searches. The
//! caller decides when to run a pass and where the store lives, through
//! `SyncHost`.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::http::{Request, Response, StatusError, Transport};
use crate::is_excluded;
use crate::model::{
    Checks, MergeState, OnClose, Provider, RemoteItem, RemoteKind, RemoteRef, RemoteState, Store,
    SyncEvent,
};

const USER_AGENT: &str = concat!("lunch-tray/", env!("CARGO_PKG_VERSION"));
const MAX_PAGES: usize = 5;
/// Pull requests per GraphQL request. GitHub gives a query about ten
/// seconds; bigger batches across many repositories time out with a 502.
const GRAPHQL_BATCH: usize = 15;

#[async_trait(?Send)]
pub trait Forge {
    fn provider(&self) -> Provider;
    fn host(&self) -> String;
    /// Every open item that matches the configured queries.
    async fn fetch_open(&self) -> Result<Vec<RemoteItem>>;
    /// One item by number, whatever its state.
    async fn fetch_one(&self, owner: &str, repo: &str, number: u64) -> Result<RemoteItem>;
    /// Repository patterns to ignore.
    fn exclude(&self) -> &[String];
    /// True when the host's quota is nearly spent; optional work waits.
    fn quota_low(&self) -> bool {
        false
    }
    /// What the host said about its rate limits, to keep across restarts.
    fn limits(&self) -> Limits {
        Limits::default()
    }
    /// Take back limits from an earlier `limits()`.
    fn restore_limits(&self, _limits: Limits) {}
    /// CI and merge state for these open pull requests. Items the forge
    /// cannot answer for are simply left out.
    async fn fetch_checks(&self, prs: &[RemoteRef]) -> Result<Vec<(String, Checks)>>;
}

/// Count CI contexts as passed, failed, and pending, plus the total.
/// Skipped, neutral, and cancelled jobs are left out of the total.
fn tally(states: impl IntoIterator<Item = CheckOutcome>) -> (u32, u32, u32, u32) {
    let (mut passed, mut failed, mut pending) = (0, 0, 0);
    for s in states {
        match s {
            CheckOutcome::Passed => passed += 1,
            CheckOutcome::Failed => failed += 1,
            CheckOutcome::Pending => pending += 1,
            CheckOutcome::Skipped => {}
        }
    }
    (passed, failed, pending, passed + failed + pending)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckOutcome {
    Passed,
    Failed,
    Pending,
    /// Skipped, neutral, or cancelled.
    Skipped,
}

/// One HTTP client per account, keeping GitHub's ETags and rate limits.
pub struct Http {
    transport: Box<dyn Transport>,
    /// ETag and body of the last answer per GET URL. A matching `304`
    /// costs nothing against GitHub's rate limit.
    etags: Mutex<HashMap<String, (String, String)>>,
    /// GitHub's own count per quota, read from the latest answer's headers.
    quotas: Mutex<HashMap<String, Quota>>,
    /// Set when the host said to stop; nothing is sent before it.
    paused_until: Mutex<Option<DateTime<Utc>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Quota {
    limit: u64,
    remaining: u64,
    reset: DateTime<Utc>,
}

/// A host's rate limits as last heard: the quota per resource, and when
/// it said to stop until. Held in memory; the Worker, which restarts often,
/// saves it between passes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    #[serde(default)]
    quotas: HashMap<String, Quota>,
    #[serde(default)]
    paused_until: Option<DateTime<Utc>>,
}

/// GitHub's rate-limit headers, when present.
fn parse_quota(resp: &Response) -> Option<(String, Quota)> {
    let get = |k: &str| resp.header(k);
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
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Http {
            transport,
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

    fn limits(&self) -> Limits {
        Limits {
            quotas: self
                .quotas
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            paused_until: self.paused(),
        }
    }

    fn restore_limits(&self, limits: Limits) {
        *self.quotas.lock().unwrap_or_else(|e| e.into_inner()) = limits.quotas;
        *self.paused_until.lock().unwrap_or_else(|e| e.into_inner()) = limits.paused_until;
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

    /// GET `url` with `headers` and decode JSON.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        headers: &[(&str, String)],
    ) -> Result<T> {
        self.request_json(url, headers, None).await
    }

    /// POST a JSON body and decode the JSON reply.
    async fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        headers: &[(&str, String)],
        body: &serde_json::Value,
    ) -> Result<T> {
        self.request_json(url, headers, Some(body)).await
    }

    async fn request_json<T: serde::de::DeserializeOwned>(
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
        let cached = if body.is_none() {
            self.etags
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(url)
                .cloned()
        } else {
            None
        };
        let mut req_headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        req_headers.push(("User-Agent".into(), USER_AGENT.into()));
        if let Some((etag, _)) = &cached {
            req_headers.push(("If-None-Match".into(), etag.clone()));
        }
        if body.is_some() {
            req_headers.push(("Content-Type".into(), "application/json".into()));
        }
        let resp = self
            .transport
            .send(Request {
                method,
                url: url.to_string(),
                headers: req_headers,
                body: body.map(|b| b.to_string()),
            })
            .await
            .with_context(|| format!("{method} {url}"))?;
        let status = resp.status;
        if let Some((resource, quota)) = parse_quota(&resp) {
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
        if status == 403 || status == 429 {
            // GitHub says 403 for its primary limit and 403 or 429 for
            // the secondary one, with a Retry-After. Stop until then.
            let retry_after = resp
                .header("retry-after")
                .and_then(|v| v.parse::<i64>().ok());
            let exhausted = parse_quota(&resp)
                .filter(|(_, q)| q.remaining == 0)
                .map(|(_, q)| q.reset);
            let message = serde_json::from_str::<serde_json::Value>(&resp.body)
                .ok()
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
            return Err(StatusError(status)).with_context(|| format!("{method} {url}: {message}"));
        }
        if status >= 400 {
            return Err(StatusError(status)).with_context(|| format!("{method} {url}"));
        }
        if body.is_none()
            && let Some(etag) = resp.header("etag")
        {
            self.etags
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(url.to_string(), (etag.to_string(), resp.body.clone()));
        }
        serde_json::from_str::<T>(&resp.body).with_context(|| format!("decode {url}"))
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

    async fn checks_batch(&self, batch: &[RemoteRef]) -> Result<Vec<(String, Checks)>> {
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
             ... on CheckRun { databaseId name status conclusion startedAt completedAt \
             checkSuite { workflowRun { databaseId workflow { name } } } } \
             ... on StatusContext { context state createdAt } } } } } } } }",
        );
        let reply: GqlReply = self
            .http
            .post_json(
                &self.graphql_url(),
                &[("Authorization", format!("Bearer {}", self.token))],
                &serde_json::json!({ "query": query }),
            )
            .await?;
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

    pub fn new(
        api_url: &str,
        token: String,
        queries: Vec<String>,
        exclude: Vec<String>,
        transport: Box<dyn Transport>,
    ) -> Self {
        GitHub {
            api_url: api_url.trim_end_matches('/').to_string(),
            host: github_host(api_url),
            token,
            queries,
            exclude,
            http: Http::new(transport),
        }
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        self.http
            .get_json(
                url,
                &[
                    ("Authorization", format!("Bearer {}", self.token)),
                    ("Accept", "application/vnd.github+json".to_string()),
                    ("X-GitHub-Api-Version", "2022-11-28".to_string()),
                ],
            )
            .await
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
                query_misses: 0,
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
                query_misses: 0,
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

/// Host name used in task ids and labels: `github.com` for the public API,
/// else the host of `api_url`.
pub fn github_host(api_url: &str) -> String {
    let trimmed = api_url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = trimmed.split('/').next().unwrap_or(trimmed);
    if host == "api.github.com" {
        "github.com".to_string()
    } else {
        host.to_string()
    }
}

/// Default GitHub searches: your pull requests, the ones waiting for your
/// review, and whatever is assigned to you.
pub fn default_github_queries() -> Vec<String> {
    vec![
        "is:open is:pr author:@me archived:false".into(),
        "is:open is:pr review-requested:@me archived:false".into(),
        "is:open assignee:@me archived:false".into(),
    ]
}

#[async_trait(?Send)]
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

    fn limits(&self) -> Limits {
        self.http.limits()
    }

    fn restore_limits(&self, limits: Limits) {
        self.http.restore_limits(limits);
    }

    /// Batches of `GRAPHQL_BATCH` pull requests per GraphQL request: each
    /// alias asks for the merge state and the check rollup of the head
    /// commit. A batch that fails is split in half and retried, down to
    /// single pull requests, so one slow repository cannot stall the rest.
    async fn fetch_checks(&self, prs: &[RemoteRef]) -> Result<Vec<(String, Checks)>> {
        let mut out = Vec::new();
        let mut queue: std::collections::VecDeque<&[RemoteRef]> =
            prs.chunks(GRAPHQL_BATCH).collect();
        while let Some(batch) = queue.pop_front() {
            match self.checks_batch(batch).await {
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
    async fn fetch_open(&self) -> Result<Vec<RemoteItem>> {
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
                let reply: GqlReply = self
                    .http
                    .post_json(
                        &self.graphql_url(),
                        &[("Authorization", format!("Bearer {}", self.token))],
                        &serde_json::json!({ "query": QUERY, "variables": { "q": q, "after": after } }),
                    )
                    .await?;
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

    async fn fetch_one(&self, owner: &str, repo: &str, number: u64) -> Result<RemoteItem> {
        let url = format!("{}/repos/{owner}/{repo}/issues/{number}", self.api_url);
        let it: GhIssue = self.get(&url).await?;
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
        database_id: Option<i64>,
        name: String,
        status: String,
        conclusion: Option<String>,
        started_at: Option<String>,
        completed_at: Option<String>,
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
#[serde(rename_all = "camelCase")]
struct GqlWorkflowRun {
    database_id: Option<i64>,
    workflow: GqlWorkflow,
}

#[derive(Deserialize)]
struct GqlWorkflow {
    name: String,
}

/// A workflow contributes only its newest run, so jobs removed from a newer
/// run do not linger from an older one. A re-run keeps its workflow run id,
/// so the newest check run breaks the tie for each job. Jobs are told apart
/// by workflow and name, since two workflows can both have a job called
/// `build`.
fn latest_per_name(nodes: &[GqlContext]) -> Vec<&GqlContext> {
    let mut newest_runs: HashMap<&str, i64> = HashMap::new();
    for node in nodes {
        let GqlContext::CheckRun { check_suite, .. } = node else {
            continue;
        };
        let Some(run) = check_suite.as_ref().and_then(|c| c.workflow_run.as_ref()) else {
            continue;
        };
        if let Some(id) = run.database_id {
            newest_runs
                .entry(run.workflow.name.as_str())
                .and_modify(|newest| *newest = (*newest).max(id))
                .or_insert(id);
        }
    }

    let millis = |s: &Option<String>| {
        s.as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map_or(0, |t| t.timestamp_millis())
    };
    let mut latest: std::collections::BTreeMap<String, ((i64, i64), &GqlContext)> =
        std::collections::BTreeMap::new();
    for node in nodes {
        let (name, recency) = match node {
            GqlContext::CheckRun {
                database_id,
                name,
                started_at,
                completed_at,
                check_suite,
                ..
            } => {
                let run = check_suite.as_ref().and_then(|c| c.workflow_run.as_ref());
                let workflow = run.map(|w| w.workflow.name.as_str()).unwrap_or("");
                if run.is_some_and(|run| {
                    run.database_id
                        .is_some_and(|id| newest_runs.get(workflow) != Some(&id))
                }) {
                    continue;
                }
                let first = run.and_then(|w| w.database_id).unwrap_or_else(|| {
                    millis(if completed_at.is_some() {
                        completed_at
                    } else {
                        started_at
                    })
                });
                (
                    format!("{workflow} / {name}"),
                    (first, database_id.unwrap_or(0)),
                )
            }
            GqlContext::StatusContext {
                context,
                created_at,
                ..
            } => (context.clone(), (millis(created_at), 0)),
            GqlContext::Other => continue,
        };
        match latest.get(&name) {
            Some((seen, _)) if *seen >= recency => {}
            _ => {
                latest.insert(name, (recency, node));
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
                // As Paseo maps them. A conclusion not listed here, such as
                // STALE or STARTUP_FAILURE, stays pending.
                match conclusion.as_deref() {
                    Some("SUCCESS") => CheckOutcome::Passed,
                    Some("FAILURE" | "TIMED_OUT" | "ACTION_REQUIRED") => CheckOutcome::Failed,
                    Some("CANCELLED" | "SKIPPED" | "NEUTRAL") => CheckOutcome::Skipped,
                    _ => CheckOutcome::Pending,
                }
            }
            GqlContext::StatusContext { state, .. } => match state.as_str() {
                "SUCCESS" => CheckOutcome::Passed,
                "FAILURE" | "ERROR" => CheckOutcome::Failed,
                _ => CheckOutcome::Pending,
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
                tally(runs.iter().map(|c| c.outcome()))
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

/// How the last passes went, for the window, the tray, and the MCP tools.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStatus {
    pub in_progress: bool,
    pub last_finished: Option<DateTime<Utc>>,
    pub errors: Vec<String>,
}

/// Where a sync pass reads and writes tasks. The desktop app keeps the
/// store behind a mutex; the Worker keeps it in a Durable Object. A pass
/// never holds the store across a network call, so other changes can come
/// in meanwhile.
pub trait SyncHost {
    fn with_store<R>(&self, f: impl FnOnce(&mut Store) -> R) -> R;
    /// Persist the store after the pass changed it.
    fn save(&self);
    /// Something visible changed.
    fn changed(&self);
    /// A message for the user.
    fn notice(&self, error: bool, text: String);
}

#[derive(Clone, Copy, Debug)]
pub struct SyncOptions {
    pub on_close: OnClose,
    /// Fetch CI results and merge state.
    pub checks: bool,
    /// Tracked open items the searches left out are fetched one by one,
    /// at most this many per pass, in turns.
    pub max_individual_checks: usize,
}

/// Second phase: CI and merge state for the open pull requests that are
/// due, see `Store::checks_due`.
async fn sync_checks<H: SyncHost>(forge: &dyn Forge, host: &H) -> Result<()> {
    let due = host.with_store(|s| s.checks_due(forge.provider(), &forge.host(), Utc::now()));
    if due.is_empty() {
        return Ok(());
    }
    log::debug!("checking CI state of {} pull requests", due.len());
    let results = forge.fetch_checks(&due).await?;
    let changed = host.with_store(|s| {
        let mut changed = false;
        for (key, checks) in results {
            changed |= s.set_checks(&key, checks);
        }
        changed
    });
    // Always save: checked_at moved even when the numbers did not.
    host.save();
    if changed {
        host.changed();
    }
    Ok(())
}

/// A 401 or 403: the token is the problem, not the request.
fn is_auth_error(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<StatusError>(),
        Some(StatusError(401 | 403))
    )
}

/// A 404 or 410 from an individual fetch.
fn is_gone(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<StatusError>(),
        Some(StatusError(404 | 410))
    )
}

/// One pass over every forge. Returns the errors, one line per forge that
/// failed; the caller shows them and keeps the sync status.
///
/// `round` rotates which tracked items get an individual check when there
/// are more than `max_individual_checks` of them.
pub async fn sync_once<H: SyncHost>(
    forges: &[Box<dyn Forge>],
    host: &H,
    opts: SyncOptions,
    round: usize,
) -> Vec<String> {
    let mut errors = Vec::new();
    for forge in forges {
        if let Err(e) = sync_forge(forge.as_ref(), host, opts, round).await {
            let msg = format!("{} {}: {e:#}", forge.provider().label(), forge.host());
            log::warn!("{msg}");
            errors.push(msg);
        }
        if opts.checks && forge.quota_low() {
            log::info!(
                "{} quota is low, skipping CI checks this poll",
                forge.host()
            );
        } else if opts.checks
            && let Err(e) = sync_checks(forge.as_ref(), host).await
        {
            log::warn!(
                "{} {} checks: {e:#}",
                forge.provider().label(),
                forge.host()
            );
        }
    }
    errors
}

async fn sync_forge<H: SyncHost>(
    forge: &dyn Forge,
    host: &H,
    opts: SyncOptions,
    round: usize,
) -> Result<()> {
    let mut items = forge.fetch_open().await?;
    let excluded = |r: &RemoteRef| is_excluded(forge.exclude(), &r.owner, &r.repo);
    items.retain(|i| !excluded(&i.r));
    let seen: HashSet<String> = items.iter().map(|i| i.r.key()).collect();

    // Tracked open items that the queries no longer return: check each one
    // so we notice closes and merges.
    let mut to_check: Vec<(RemoteRef, String)> = host.with_store(|s| {
        s.tasks()
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
    });
    if !to_check.is_empty() && forge.quota_low() {
        log::info!(
            "{} quota is low, skipping {} individual checks",
            forge.host(),
            to_check.len()
        );
        to_check.clear();
    }
    let max = opts.max_individual_checks;
    if to_check.len() > max {
        let start = (round * max) % to_check.len();
        to_check.rotate_left(start);
        to_check.truncate(max);
    }
    for (r, title) in to_check {
        match forge.fetch_one(&r.owner, &r.repo, r.number).await {
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

    let (put_away, events) = host.with_store(|s| {
        // Tasks from repositories that are now excluded go to the archive.
        let put_away = s.archive_matching(|t| {
            t.remote().is_some_and(|r| {
                r.provider == forge.provider() && r.host == forge.host() && excluded(r)
            })
        });
        let events = s.apply_remote(opts.on_close, &items);
        (put_away, events)
    });
    if !events.is_empty() || !put_away.is_empty() {
        host.save();
        host.changed();
    }
    for label in put_away {
        host.notice(false, format!("Excluded repository: {label}"));
    }
    for ev in events {
        host.notice(false, event_text(&ev));
    }
    Ok(())
}

/// The notice for a sync event.
pub fn event_text(ev: &SyncEvent) -> String {
    match ev {
        SyncEvent::New { label, .. } => format!("New: {label}"),
        SyncEvent::Closed {
            label,
            merged: true,
            ..
        } => format!("Merged: {label}"),
        SyncEvent::Closed {
            label,
            merged: false,
            ..
        } => format!("Closed: {label}"),
        SyncEvent::ReopenedRemotely { label, .. } => format!("Reopened: {label}"),
        SyncEvent::Woken { label, .. } => format!("Activity: {label}"),
        SyncEvent::Released { label, .. } => format!("Not waiting on you: {label}"),
        SyncEvent::Requested { label, .. } => format!("Needs you: {label}"),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    struct NoNet;

    #[async_trait(?Send)]
    impl Transport for NoNet {
        async fn send(&self, _: Request) -> Result<Response> {
            bail!("no network in tests")
        }
    }

    /// Answers GraphQL searches with one open pull request, and check
    /// queries with a passing run.
    struct FakeGitHub {
        sent: RefCell<Vec<String>>,
    }

    #[async_trait(?Send)]
    impl Transport for FakeGitHub {
        async fn send(&self, req: Request) -> Result<Response> {
            let body = req.body.unwrap_or_default();
            assert!(req.headers.iter().any(|(k, _)| k == "User-Agent"));
            self.sent.borrow_mut().push(body.clone());
            let json = if body.contains("search(") {
                r#"{"data": {"search": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": [
                    {"__typename": "PullRequest", "number": 5, "title": "Add MCP", "url": "https://github.com/o/r/pull/5",
                     "state": "OPEN", "updatedAt": "2026-01-02T03:04:05Z", "isDraft": false,
                     "author": {"login": "me"}, "repository": {"nameWithOwner": "o/r"}}]}}}"#
            } else {
                r#"{"data": {"p0": {"pullRequest": {"headRefOid": "abc", "mergeable": "MERGEABLE",
                    "mergeStateStatus": "CLEAN", "commits": {"nodes": [{"commit": {"statusCheckRollup": {
                    "state": "SUCCESS", "contexts": {"nodes": [{"__typename": "CheckRun", "databaseId": 1,
                    "name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}]}}}}]}}}}}"#
            };
            Ok(Response {
                status: 200,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: json.into(),
            })
        }
    }

    struct TestHost {
        store: RefCell<Store>,
        saves: RefCell<usize>,
        notices: RefCell<Vec<String>>,
    }

    impl SyncHost for TestHost {
        fn with_store<R>(&self, f: impl FnOnce(&mut Store) -> R) -> R {
            f(&mut self.store.borrow_mut())
        }
        fn save(&self) {
            *self.saves.borrow_mut() += 1;
        }
        fn changed(&self) {}
        fn notice(&self, _error: bool, text: String) {
            self.notices.borrow_mut().push(text);
        }
    }

    #[test]
    fn a_pass_adds_new_items_and_their_checks() {
        let gh = GitHub::new(
            "https://api.github.com",
            "t".into(),
            vec!["is:open is:pr author:@me".into()],
            vec![],
            Box::new(FakeGitHub {
                sent: RefCell::new(Vec::new()),
            }),
        );
        let forges: Vec<Box<dyn Forge>> = vec![Box::new(gh)];
        let host = TestHost {
            store: RefCell::new(Store::in_memory()),
            saves: RefCell::new(0),
            notices: RefCell::new(Vec::new()),
        };
        let opts = SyncOptions {
            on_close: OnClose::Settle,
            checks: true,
            max_individual_checks: 10,
        };
        let errors = pollster::block_on(sync_once(&forges, &host, opts, 0));
        assert!(errors.is_empty(), "{errors:?}");
        let store = host.store.borrow();
        let task = store.get("github:github.com/o/r#5").expect("task added");
        assert_eq!(task.title, "Add MCP");
        let checks = task
            .remote()
            .unwrap()
            .checks
            .as_ref()
            .expect("checks fetched");
        assert_eq!((checks.passed, checks.total), (1, 1));
        assert_eq!(host.notices.borrow().as_slice(), ["New: o/r #5"]);
        assert!(*host.saves.borrow() >= 2);
    }

    #[test]
    fn github_search_item_converts() {
        let gh = GitHub {
            api_url: "https://api.github.com".into(),
            host: "github.com".into(),
            token: String::new(),
            queries: vec![],
            exclude: vec![],
            http: Http::new(Box::new(NoNet)),
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
            http: Http::new(Box::new(NoNet)),
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
        // is left out of the total.
        assert_eq!((c.passed, c.failed, c.pending, c.total), (4, 0, 0, 4));
    }

    #[test]
    fn jobs_absent_from_the_latest_workflow_run_do_not_count() {
        let json = r#"{"headRefOid": "abc", "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "SUCCESS",
            "contexts": {"nodes": [
                {"__typename": "CheckRun", "databaseId": 11, "name": "build", "status": "COMPLETED", "conclusion": "FAILURE",
                 "checkSuite": {"workflowRun": {"databaseId": 7, "workflow": {"name": "CI"}}}},
                {"__typename": "CheckRun", "databaseId": 12, "name": "release", "status": "COMPLETED", "conclusion": "SUCCESS",
                 "checkSuite": {"workflowRun": {"databaseId": 7, "workflow": {"name": "CI"}}}},
                {"__typename": "CheckRun", "databaseId": 13, "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS",
                 "checkSuite": {"workflowRun": {"databaseId": 8, "workflow": {"name": "CI"}}}}]}}}}]}}"#;
        let c = serde_json::from_str::<GqlPr>(json)
            .unwrap()
            .into_checks(Utc::now());
        assert_eq!((c.passed, c.failed, c.pending, c.total), (1, 0, 0, 1));
    }

    #[test]
    fn a_queued_rerun_replaces_the_failed_run() {
        // A re-run keeps its workflow run id and has no start time while
        // queued. The newer check run id still makes it the latest.
        let json = r#"{"headRefOid": "abc", "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "PENDING",
            "contexts": {"nodes": [
                {"__typename": "CheckRun", "databaseId": 11, "name": "build", "status": "COMPLETED", "conclusion": "FAILURE",
                 "startedAt": "2026-10-22T10:00:00Z", "completedAt": "2026-10-22T10:05:00Z",
                 "checkSuite": {"workflowRun": {"databaseId": 7, "workflow": {"name": "CI"}}}},
                {"__typename": "CheckRun", "databaseId": 12, "name": "build", "status": "QUEUED", "conclusion": null,
                 "startedAt": null, "completedAt": null,
                 "checkSuite": {"workflowRun": {"databaseId": 7, "workflow": {"name": "CI"}}}}]}}}}]}}"#;
        let c = serde_json::from_str::<GqlPr>(json)
            .unwrap()
            .into_checks(Utc::now());
        assert_eq!((c.passed, c.failed, c.pending, c.total), (0, 0, 1, 1));
    }

    #[test]
    fn conclusions_map_as_in_paseo() {
        let run = |conclusion: &str| GqlContext::CheckRun {
            database_id: None,
            name: "x".into(),
            status: "COMPLETED".into(),
            conclusion: Some(conclusion.into()),
            started_at: None,
            completed_at: None,
            check_suite: None,
        };
        use CheckOutcome::*;
        for (conclusion, outcome) in [
            ("SUCCESS", Passed),
            ("FAILURE", Failed),
            ("TIMED_OUT", Failed),
            ("ACTION_REQUIRED", Failed),
            ("CANCELLED", Skipped),
            ("SKIPPED", Skipped),
            ("NEUTRAL", Skipped),
            ("STALE", Pending),
            ("STARTUP_FAILURE", Pending),
        ] {
            assert_eq!(run(conclusion).outcome(), outcome, "{conclusion}");
        }
        let status = |state: &str| GqlContext::StatusContext {
            context: "x".into(),
            state: state.into(),
            created_at: None,
        };
        assert_eq!(status("ERROR").outcome(), Failed);
        assert_eq!(status("EXPECTED").outcome(), Pending);
    }

    #[test]
    fn skipped_jobs_do_not_count() {
        let (p, f, pe, total) = tally([
            CheckOutcome::Passed,
            CheckOutcome::Skipped,
            CheckOutcome::Failed,
            CheckOutcome::Skipped,
            CheckOutcome::Pending,
        ]);
        assert_eq!((p, f, pe, total), (1, 1, 1, 3));
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
        let headers = [
            ("X-RateLimit-Resource", "search"),
            ("x-ratelimit-limit", "30"),
            ("x-ratelimit-remaining", "2"),
            ("x-ratelimit-reset", "1800000000"),
        ];
        let resp = Response {
            status: 200,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: String::new(),
        };
        let (resource, q) = parse_quota(&resp).unwrap();
        assert_eq!(resource, "search");
        assert_eq!((q.limit, q.remaining), (30, 2));
        assert_eq!(q.reset.timestamp(), 1_800_000_000);
        let empty = Response {
            status: 200,
            headers: vec![],
            body: String::new(),
        };
        assert!(parse_quota(&empty).is_none());

        let http = Http::new(Box::new(NoNet));
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

        // Saved and restored, as the Worker does across restarts.
        let saved = serde_json::to_string(&http.limits()).unwrap();
        let fresh = Http::new(Box::new(NoNet));
        fresh.restore_limits(serde_json::from_str(&saved).unwrap());
        assert!(fresh.quota_low());
        assert!(fresh.paused().is_some());
    }

    #[test]
    fn graphql_url_sits_next_to_the_rest_api() {
        let mut gh = GitHub {
            api_url: "https://api.github.com".into(),
            host: "github.com".into(),
            token: String::new(),
            queries: vec![],
            exclude: vec![],
            http: Http::new(Box::new(NoNet)),
        };
        assert_eq!(gh.graphql_url(), "https://api.github.com/graphql");
        gh.api_url = "https://ghe.example.com/api/v3".into();
        assert_eq!(gh.graphql_url(), "https://ghe.example.com/api/graphql");
    }
}
