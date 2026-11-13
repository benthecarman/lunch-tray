//! Lunch Tray on Cloudflare Workers.
//!
//! One Durable Object, `Tray`, owns the task list in SQLite: one row per
//! task. An alarm runs the GitHub sync every `POLL_INTERVAL_SECS`. The
//! object also serves the MCP tools at `/mcp` and the API the desktop app
//! uses at `/api/*`, see `lunch_tray_core::api`. The Worker in front checks
//! the bearer token and hands everything else to the object.
//!
//! The object goes to sleep between requests, so it does not count on
//! memory. Every change takes the next revision number, kept in SQLite, and
//! each task row keeps the revision of its last change. The app asks for
//! what changed after the revision it has: when nothing did, that costs one
//! row read. The whole list is read into memory only for ops, the sync,
//! and MCP.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use lunch_tray_core::api::{self, OpBatch, OpResult, RemoteNotice};
use lunch_tray_core::http::{Request as HttpRequest, Response as HttpResponse, Transport};
use lunch_tray_core::mcp::{self, Backend, Incoming};
use lunch_tray_core::model::{OnClose, Snapshot, Store, Task};
use lunch_tray_core::ops::{Applied, Op};
use lunch_tray_core::sync::{
    Forge, GitHub, SyncHost, SyncOptions, SyncStatus, default_github_queries, sync_once,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use worker::{
    Date, DurableObject, Env, Fetch, Headers, Method, Request, RequestInit, Response, Result,
    SqlStorage, SqlStorageValue, State, durable_object, event, wasm_bindgen::JsValue,
};

type Forges = Rc<Vec<Box<dyn Forge>>>;

/// How long the answer to an op batch is kept for resends.
const BATCH_ANSWER_MS: i64 = 24 * 60 * 60 * 1000;

/// Notices kept for the desktop app to pick up.
const NOTICES_KEPT: u32 = 50;

/// Bumped when the tables change shape; see `Core::new`.
const SCHEMA: u64 = 2;

#[event(start)]
fn start() {
    let _ = log::set_logger(&ConsoleLog);
    log::set_max_level(log::LevelFilter::Info);
}

struct ConsoleLog;

impl log::Log for ConsoleLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::max_level()
    }

    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            worker::console_log!("[{}] {}", r.level(), r.args());
        }
    }

    fn flush(&self) {}
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: worker::Context) -> Result<Response> {
    if req.path() == "/" {
        return Response::ok("lunch-tray\n");
    }
    let token = env.secret("API_TOKEN")?.to_string();
    let given = req.headers().get("Authorization")?.unwrap_or_default();
    let given = given.strip_prefix("Bearer ").unwrap_or("").trim();
    if token.is_empty() || !mcp::same_secret(given.as_bytes(), token.as_bytes()) {
        let headers = Headers::new();
        headers.set("WWW-Authenticate", "Bearer")?;
        return Ok(Response::empty()?.with_status(401).with_headers(headers));
    }
    let stub = env
        .durable_object("TRAY")?
        .id_from_name("main")?
        .get_stub()?;
    stub.fetch_with_request(req).await
}

#[durable_object]
pub struct Tray {
    state: State,
    env: Env,
    core: Rc<Core>,
}

impl DurableObject for Tray {
    fn new(state: State, env: Env) -> Self {
        let core = Core::new(state.storage().sql()).expect("open the database");
        Tray {
            state,
            env,
            core: Rc::new(core),
        }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let path = req.path();
        // The app's polls only read; everything else makes sure the sync
        // alarm is booked, which also starts it on a new Worker.
        if !(req.method() == Method::Get && path == api::STATE)
            && self.state.storage().get_alarm().await?.is_none()
        {
            self.state.storage().set_alarm(1000).await?;
        }
        let resp = match (req.method(), path.as_str()) {
            (_, mcp::PATH) => self.mcp(&mut req).await,
            (Method::Get, api::STATE) => self.api_state(&req),
            (Method::Post, api::OPS) => self.api_ops(&mut req).await,
            (Method::Post, api::SYNC) => {
                self.core.sync_requested.set(true);
                json_response(r#"{"requested":true}"#.into())
            }
            (Method::Put, api::IMPORT) => self.api_import(&mut req).await,
            _ => Ok(Response::empty()?.with_status(404)),
        };
        if self.core.sync_requested.take() {
            self.state.storage().set_alarm(0).await?;
        }
        resp
    }

    async fn alarm(&self) -> Result<Response> {
        let cfg = WorkerConfig::from_env(&self.env);
        // Book the next run first, so a failing pass does not end the loop.
        self.state
            .storage()
            .set_alarm(Duration::from_secs(cfg.poll_interval_secs))
            .await?;
        self.core.sync(&self.env, &cfg).await;
        Response::ok("synced")
    }
}

impl Tray {
    async fn mcp(&self, req: &mut Request) -> Result<Response> {
        let headers: Vec<(String, String)> = req.headers().entries().collect();
        let method = req.method().to_string();
        let body = if req.method() == Method::Post {
            req.text().await?
        } else {
            String::new()
        };
        if body.len() as u64 > mcp::MAX_BODY {
            return Ok(Response::empty()?.with_status(413));
        }
        // The Worker checked the token already.
        let server = mcp::Server::new(CoreRef(self.core.clone()), None, Vec::new());
        let reply = server.handle(&Incoming {
            method: &method,
            path: mcp::PATH,
            headers: &headers,
            body: body.as_bytes(),
        });
        let out = Headers::new();
        for (k, v) in &reply.headers {
            out.set(k, v)?;
        }
        let resp = match &reply.body {
            Some(b) => {
                out.set("Content-Type", "application/json")?;
                Response::ok(b.to_string())?
            }
            None => Response::empty()?,
        };
        Ok(resp.with_status(reply.status).with_headers(out))
    }

    /// What changed after `?since=`, straight from the rows: tasks are
    /// passed through as stored, never parsed.
    fn api_state(&self, req: &Request) -> Result<Response> {
        let since: u64 = req
            .url()?
            .query_pairs()
            .find(|(k, _)| k == "since")
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        let core = &self.core;
        let rev = core.rev()?;
        if since != 0 && since == rev {
            let headers = Headers::new();
            headers.set("Cache-Control", "no-store")?;
            return Ok(Response::empty()?.with_status(304).with_headers(headers));
        }
        let full_since: u64 = core.meta_u64("full_since")?;
        let full = since == 0 || since > rev || since < full_since;
        let sql = &core.sql;
        let tasks: Vec<JsonRow> = if full {
            sql.exec("SELECT json FROM tasks", None)?.to_array()?
        } else {
            sql.exec("SELECT json FROM tasks WHERE rev > ?", vec![int(since)])?
                .to_array()?
        };
        let removed: Vec<IdRow> = if full {
            Vec::new()
        } else {
            sql.exec("SELECT id FROM removed WHERE rev > ?", vec![int(since)])?
                .to_array()?
        };
        // On first contact, what happened before is old news.
        let notices: Vec<NoticeRow> = if since == 0 {
            Vec::new()
        } else {
            sql.exec(
                "SELECT rev, error, text FROM notices WHERE rev > ? ORDER BY rev",
                vec![int(since)],
            )?
            .to_array()?
        };
        let out = DeltaOut {
            rev,
            full,
            tasks: tasks
                .into_iter()
                .filter_map(|r| RawValue::from_string(r.json).ok())
                .collect(),
            removed: removed.into_iter().map(|r| r.id).collect(),
            next_manual_id: core.meta_u64("next_manual_id")?,
            sync: core.sync_status()?,
            notices: notices
                .into_iter()
                .map(|n| RemoteNotice {
                    seq: n.rev as u64,
                    error: n.error != 0,
                    text: n.text,
                })
                .collect(),
        };
        json_response(serde_json::to_string(&out).map_err(|e| e.to_string())?)
    }

    async fn api_ops(&self, req: &mut Request) -> Result<Response> {
        let batch: OpBatch = match serde_json::from_str(&req.text().await?) {
            Ok(b) => b,
            Err(e) => return Response::error(format!("bad ops: {e}"), 400),
        };
        let sql = &self.core.sql;
        // A resend after a lost answer: answer again, apply nothing.
        if let Some(results) = first_value(sql.exec(
            "SELECT value FROM batches WHERE id = ?",
            vec![batch.id.as_str().into()],
        )?)? {
            log::info!("batch {} again, answered from the record", batch.id);
            return json_response(results);
        }
        let results: Vec<OpResult> = batch
            .ops
            .into_iter()
            .map(|op| self.core.apply(op))
            .collect();
        let text = serde_json::to_string(&results).unwrap_or_default();
        let now = Date::now().as_millis() as i64;
        sql.exec(
            "INSERT OR REPLACE INTO batches (id, value, at) VALUES (?, ?, ?)",
            vec![batch.id.into(), text.as_str().into(), now.into()],
        )?;
        sql.exec(
            "DELETE FROM batches WHERE at < ?",
            vec![(now - BATCH_ANSWER_MS).into()],
        )?;
        json_response(text)
    }

    async fn api_import(&self, req: &mut Request) -> Result<Response> {
        let replace = req
            .url()?
            .query_pairs()
            .any(|(k, v)| k == "replace" && v == "1");
        let snap: Snapshot = match serde_json::from_str(&req.text().await?) {
            Ok(s) => s,
            Err(e) => return Response::error(format!("bad snapshot: {e}"), 400),
        };
        if !replace && self.core.with_store(|s| !s.tasks().is_empty()) {
            return Response::error("the Worker already has tasks", 409);
        }
        let n = snap.tasks.len();
        self.core.with_store(|s| *s = Store::from_snapshot(snap));
        self.core.persist();
        // Copies from before the import start over.
        let rev = self.core.rev()?;
        self.core.set_meta("full_since", &rev.to_string());
        log::info!("imported {n} tasks");
        json_response(format!(r#"{{"imported":{n}}}"#))
    }
}

/// `api::Delta`, with the tasks as stored JSON.
#[derive(Serialize)]
struct DeltaOut {
    rev: u64,
    full: bool,
    tasks: Vec<Box<RawValue>>,
    removed: Vec<String>,
    next_manual_id: u64,
    sync: SyncStatus,
    notices: Vec<RemoteNotice>,
}

#[derive(Deserialize)]
struct Row {
    id: String,
    json: String,
}

#[derive(Deserialize)]
struct JsonRow {
    json: String,
}

#[derive(Deserialize)]
struct IdRow {
    id: String,
}

#[derive(Deserialize)]
struct NoticeRow {
    rev: i64,
    error: i64,
    text: String,
}

#[derive(Deserialize)]
struct ValueRow {
    value: String,
}

/// The task list in memory, read on first use by an op, the sync, or MCP.
struct Loaded {
    store: Store,
    /// Each task as last written, to write only the rows that changed.
    saved: HashMap<String, String>,
    saved_next_id: u64,
}

/// What the object's handlers share. Nothing borrows across an `.await`,
/// so requests that arrive while a sync waits on GitHub are served
/// meanwhile.
struct Core {
    sql: SqlStorage,
    loaded: RefCell<Option<Loaded>>,
    /// The current revision, once read.
    rev: Cell<Option<u64>>,
    sync: RefCell<Option<SyncStatus>>,
    sync_requested: Cell<bool>,
    forges: RefCell<Option<Forges>>,
}

impl Core {
    fn new(sql: SqlStorage) -> Result<Self> {
        let core = Core {
            sql,
            loaded: RefCell::new(None),
            rev: Cell::new(None),
            sync: RefCell::new(None),
            sync_requested: Cell::new(false),
            forges: RefCell::new(None),
        };
        // `meta` does not exist yet on a new Worker.
        let schema = core.meta_u64("schema").unwrap_or(0);
        if schema < SCHEMA {
            core.migrate(schema)?;
        }
        Ok(core)
    }

    fn migrate(&self, from: u64) -> Result<()> {
        let sql = &self.sql;
        sql.exec(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            None,
        )?;
        sql.exec(
            "CREATE TABLE IF NOT EXISTS batches \
             (id TEXT PRIMARY KEY, value TEXT NOT NULL, at INTEGER NOT NULL)",
            None,
        )?;
        sql.exec(
            "CREATE TABLE IF NOT EXISTS tasks (id TEXT PRIMARY KEY, json TEXT NOT NULL)",
            None,
        )?;
        if from < 2 {
            // Schema 1 had no revisions: its rows count as revision 0, which
            // every copy fetches in full.
            sql.exec(
                "ALTER TABLE tasks ADD COLUMN rev INTEGER NOT NULL DEFAULT 0",
                None,
            )?;
            sql.exec("CREATE INDEX IF NOT EXISTS tasks_rev ON tasks (rev)", None)?;
            sql.exec(
                "CREATE TABLE IF NOT EXISTS removed (id TEXT PRIMARY KEY, rev INTEGER NOT NULL)",
                None,
            )?;
            sql.exec(
                "CREATE TABLE IF NOT EXISTS notices \
                 (rev INTEGER PRIMARY KEY, error INTEGER NOT NULL, text TEXT NOT NULL)",
                None,
            )?;
        }
        self.set_meta("schema", &SCHEMA.to_string());
        log::info!("database schema {from} -> {SCHEMA}");
        Ok(())
    }

    fn meta(&self, key: &str) -> Result<Option<String>> {
        first_value(
            self.sql
                .exec("SELECT value FROM meta WHERE key = ?", vec![key.into()])?,
        )
    }

    fn meta_u64(&self, key: &str) -> Result<u64> {
        Ok(self.meta(key)?.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    fn set_meta(&self, key: &str, value: &str) {
        if let Err(e) = self.sql.exec(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)",
            vec![key.into(), value.into()],
        ) {
            log::error!("save {key}: {e}");
        }
    }

    fn rev(&self) -> Result<u64> {
        if let Some(r) = self.rev.get() {
            return Ok(r);
        }
        let r = self.meta_u64("rev")?;
        self.rev.set(Some(r));
        Ok(r)
    }

    /// Take the next revision for a change.
    fn bump(&self) -> u64 {
        let next = self.rev().unwrap_or(0) + 1;
        self.set_meta("rev", &next.to_string());
        self.rev.set(Some(next));
        next
    }

    /// Run `f` on the task list, reading it first if needed.
    fn with_store<R>(&self, f: impl FnOnce(&mut Store) -> R) -> R {
        let mut loaded = self.loaded.borrow_mut();
        let loaded = loaded.get_or_insert_with(|| self.load());
        f(&mut loaded.store)
    }

    fn load(&self) -> Loaded {
        let rows: Vec<Row> = self
            .sql
            .exec("SELECT id, json FROM tasks", None)
            .and_then(|c| c.to_array())
            .unwrap_or_else(|e| {
                log::error!("read tasks: {e}");
                Vec::new()
            });
        let mut saved = HashMap::with_capacity(rows.len());
        let mut tasks = Vec::with_capacity(rows.len());
        for row in rows {
            match serde_json::from_str::<Task>(&row.json) {
                Ok(t) => {
                    tasks.push(t);
                    saved.insert(row.id, row.json);
                }
                Err(e) => log::error!("task {} does not parse: {e}", row.id),
            }
        }
        let next_manual_id = self.meta_u64("next_manual_id").unwrap_or(0);
        Loaded {
            store: Store::from_snapshot(Snapshot {
                tasks,
                next_manual_id,
            }),
            saved,
            saved_next_id: next_manual_id,
        }
    }

    /// Write the rows that changed since the last write, all under one new
    /// revision.
    fn persist(&self) {
        let mut guard = self.loaded.borrow_mut();
        let Some(Loaded {
            store,
            saved,
            saved_next_id,
        }) = guard.as_mut()
        else {
            return;
        };
        let mut rev = None;
        let mut take_rev = || *rev.get_or_insert_with(|| self.bump());
        let mut present = HashSet::with_capacity(store.tasks().len());
        for t in store.tasks() {
            present.insert(t.id.as_str());
            let Ok(json) = serde_json::to_string(t) else {
                continue;
            };
            if saved.get(&t.id) == Some(&json) {
                continue;
            }
            let r = take_rev();
            match self.sql.exec(
                "INSERT OR REPLACE INTO tasks (id, json, rev) VALUES (?, ?, ?)",
                vec![t.id.as_str().into(), json.as_str().into(), int(r)],
            ) {
                Ok(_) => {
                    saved.insert(t.id.clone(), json);
                }
                Err(e) => log::error!("save {}: {e}", t.id),
            }
        }
        let gone: Vec<String> = saved
            .keys()
            .filter(|id| !present.contains(id.as_str()))
            .cloned()
            .collect();
        for id in gone {
            let r = take_rev();
            let deleted = self
                .sql
                .exec("DELETE FROM tasks WHERE id = ?", vec![id.as_str().into()])
                .and_then(|_| {
                    self.sql.exec(
                        "INSERT OR REPLACE INTO removed (id, rev) VALUES (?, ?)",
                        vec![id.as_str().into(), int(r)],
                    )
                });
            match deleted {
                Ok(_) => {
                    saved.remove(&id);
                }
                Err(e) => log::error!("delete {id}: {e}"),
            }
        }
        let next_id = store.next_manual_id();
        if next_id != *saved_next_id {
            take_rev();
            self.set_meta("next_manual_id", &next_id.to_string());
            *saved_next_id = next_id;
        }
    }

    fn apply(&self, op: Op) -> OpResult {
        let applied = self.with_store(|s| op.apply(s))?;
        if applied.changed {
            self.persist();
        }
        Ok(applied)
    }

    fn sync_status(&self) -> Result<SyncStatus> {
        if let Some(s) = self.sync.borrow().as_ref() {
            return Ok(s.clone());
        }
        let s: SyncStatus = self
            .meta("sync")?
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default();
        *self.sync.borrow_mut() = Some(s.clone());
        Ok(s)
    }

    fn set_sync_status(&self, f: impl FnOnce(&mut SyncStatus)) {
        let mut s = self.sync_status().unwrap_or_default();
        f(&mut s);
        self.set_meta("sync", &serde_json::to_string(&s).unwrap_or_default());
        *self.sync.borrow_mut() = Some(s);
        self.bump();
    }

    fn push_notice(&self, error: bool, text: String) {
        let rev = self.bump();
        let saved = self
            .sql
            .exec(
                "INSERT INTO notices (rev, error, text) VALUES (?, ?, ?)",
                vec![int(rev), i64::from(error).into(), text.into()],
            )
            .and_then(|_| {
                self.sql.exec(
                    "DELETE FROM notices WHERE rev NOT IN \
                     (SELECT rev FROM notices ORDER BY rev DESC LIMIT ?)",
                    vec![i64::from(NOTICES_KEPT).into()],
                )
            });
        if let Err(e) = saved {
            log::error!("save notice: {e}");
        }
    }

    fn forges(&self, env: &Env, cfg: &WorkerConfig) -> Forges {
        if let Some(f) = self.forges.borrow().as_ref() {
            return f.clone();
        }
        let mut forges: Vec<Box<dyn Forge>> = Vec::new();
        match env.secret("GITHUB_TOKEN") {
            Ok(token) => forges.push(Box::new(GitHub::new(
                &cfg.github_api_url,
                token.to_string(),
                cfg.github_queries.clone(),
                cfg.github_exclude.clone(),
                Box::new(FetchTransport),
            ))),
            Err(_) => self.push_notice(true, "GitHub: no GITHUB_TOKEN secret".into()),
        }
        let forges = Rc::new(forges);
        *self.forges.borrow_mut() = Some(forges.clone());
        forges
    }

    async fn sync(&self, env: &Env, cfg: &WorkerConfig) {
        let forges = self.forges(env, cfg);
        if forges.is_empty() {
            return;
        }
        self.set_sync_status(|s| s.in_progress = true);
        let opts = SyncOptions {
            on_close: cfg.on_close,
            checks: cfg.checks,
            max_individual_checks: cfg.max_individual_checks,
        };
        // The object starts fresh for most passes, so take the turn for
        // individual checks from the clock rather than a counter.
        let round = (Utc::now().timestamp() as u64 / cfg.poll_interval_secs) as usize;
        let errors = sync_once(&forges, self, opts, round).await;
        let old = self.sync_status().unwrap_or_default().errors;
        let changed = old != errors;
        self.set_sync_status(|s| {
            s.in_progress = false;
            s.last_finished = Some(Utc::now());
            s.errors = errors.clone();
        });
        if changed {
            for e in errors {
                self.push_notice(true, e);
            }
        }
    }
}

/// The first `value` column of a query, if any row.
fn first_value(cursor: worker::SqlCursor) -> Result<Option<String>> {
    Ok(cursor
        .to_array::<ValueRow>()?
        .into_iter()
        .next()
        .map(|r| r.value))
}

/// Revisions are far below 2^53, where JavaScript numbers stop being exact.
fn int(n: u64) -> SqlStorageValue {
    SqlStorageValue::Integer(n as i64)
}

fn json_response(body: String) -> Result<Response> {
    let headers = Headers::new();
    // Without a JSON content type, Cloudflare does not compress.
    headers.set("Content-Type", "application/json")?;
    headers.set("Cache-Control", "no-store")?;
    Ok(Response::ok(body)?.with_headers(headers))
}

impl SyncHost for Core {
    fn with_store<R>(&self, f: impl FnOnce(&mut Store) -> R) -> R {
        Core::with_store(self, f)
    }

    fn save(&self) {
        self.persist();
    }

    /// `persist` takes the revision that tells the app.
    fn changed(&self) {}

    fn notice(&self, error: bool, text: String) {
        self.push_notice(error, text);
    }
}

struct CoreRef(Rc<Core>);

impl Backend for CoreRef {
    fn read<R>(&self, f: impl FnOnce(&Store) -> R) -> R {
        self.0.with_store(|s| f(s))
    }

    fn apply(&self, op: Op) -> std::result::Result<Applied, String> {
        self.0.apply(op)
    }

    fn request_sync(&self) -> bool {
        self.0.sync_requested.set(true);
        self.0.sync_status().unwrap_or_default().in_progress
    }

    fn sync_status(&self) -> SyncStatus {
        self.0.sync_status().unwrap_or_default()
    }
}

/// `fetch` behind the sync's transport trait.
struct FetchTransport;

#[async_trait(?Send)]
impl Transport for FetchTransport {
    async fn send(&self, req: HttpRequest) -> anyhow::Result<HttpResponse> {
        let werr = |e: worker::Error| anyhow::anyhow!("{e}");
        let headers = Headers::new();
        for (k, v) in &req.headers {
            headers.set(k, v).map_err(werr)?;
        }
        let mut init = RequestInit::new();
        init.with_method(Method::from(req.method.to_string()))
            .with_headers(headers);
        if let Some(body) = req.body {
            init.with_body(Some(JsValue::from_str(&body)));
        }
        let request = Request::new_with_init(&req.url, &init).map_err(werr)?;
        let mut resp = Fetch::Request(request).send().await.map_err(werr)?;
        let status = resp.status_code();
        let headers = resp.headers().entries().collect();
        let body = resp.text().await.map_err(werr)?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

/// Settings from `[vars]` in wrangler.toml.
struct WorkerConfig {
    poll_interval_secs: u64,
    on_close: OnClose,
    checks: bool,
    max_individual_checks: usize,
    github_api_url: String,
    github_queries: Vec<String>,
    github_exclude: Vec<String>,
}

impl WorkerConfig {
    fn from_env(env: &Env) -> Self {
        let var = |name: &str| {
            env.var(name)
                .ok()
                .map(|v| v.to_string())
                .filter(|v| !v.trim().is_empty())
        };
        let lines = |v: String| -> Vec<String> {
            v.split(['\n', ','])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        };
        WorkerConfig {
            poll_interval_secs: var("POLL_INTERVAL_SECS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(120u64)
                .max(30),
            on_close: match var("ON_CLOSE").as_deref() {
                Some("archive") => OnClose::Archive,
                _ => OnClose::Settle,
            },
            checks: var("CHECKS").is_none_or(|v| v != "false"),
            max_individual_checks: var("MAX_INDIVIDUAL_CHECKS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(20),
            github_api_url: var("GITHUB_API_URL")
                .unwrap_or_else(|| "https://api.github.com".into()),
            // Queries hold commas only inside quotes in practice, so split
            // them on new lines alone.
            github_queries: var("GITHUB_QUERIES")
                .map(|v| {
                    v.lines()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_else(default_github_queries),
            github_exclude: var("GITHUB_EXCLUDE").map(lines).unwrap_or_default(),
        }
    }
}
