//! Lunch Tray on Cloudflare Workers.
//!
//! One Durable Object, `Tray`, owns the task list: one SQLite row per
//! task, held in memory while the object is awake. An alarm runs the
//! GitHub sync every `POLL_INTERVAL_SECS`. The object also serves the MCP
//! tools at `/mcp` and the API the desktop app uses at `/api/*`, see
//! `lunch_tray_core::api`. The Worker in front checks the bearer token
//! and hands everything else to the object.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use lunch_tray_core::api::{self, OpBatch, OpResult, RemoteNotice, State as ApiState};
use lunch_tray_core::http::{Request as HttpRequest, Response as HttpResponse, Transport};
use lunch_tray_core::mcp::{self, Backend, Incoming};
use lunch_tray_core::model::{OnClose, Snapshot, Store, Task};
use lunch_tray_core::ops::{Applied, Op};
use lunch_tray_core::sync::{
    Forge, GitHub, SyncHost, SyncOptions, SyncStatus, default_github_queries, sync_once,
};
use worker::{
    Date, DurableObject, Env, Fetch, Headers, Method, Request, RequestInit, Response, Result,
    SqlStorage, State, durable_object, event, wasm_bindgen::JsValue,
};

type Forges = Rc<Vec<Box<dyn Forge>>>;

/// How long the answer to an op batch is kept for resends.
const BATCH_ANSWER_MS: i64 = 24 * 60 * 60 * 1000;

/// Notices kept for the desktop app to pick up.
const NOTICES_KEPT: usize = 50;

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
    /// Whether this instance made sure an alarm is set.
    alarm_checked: Cell<bool>,
}

impl DurableObject for Tray {
    fn new(state: State, env: Env) -> Self {
        let core = Core::load(state.storage().sql()).expect("load tasks");
        Tray {
            state,
            env,
            core: Rc::new(core),
            alarm_checked: Cell::new(false),
        }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        if !self.alarm_checked.get() {
            if self.state.storage().get_alarm().await?.is_none() {
                self.state.storage().set_alarm(1000).await?;
            }
            self.alarm_checked.set(true);
        }
        let path = req.path();
        let resp = match (req.method(), path.as_str()) {
            (_, mcp::PATH) => self.mcp(&mut req).await,
            (Method::Get, api::STATE) => self.api_state(&req),
            (Method::Post, api::OPS) => self.api_ops(&mut req).await,
            (Method::Post, api::SYNC) => {
                self.core.inner.borrow_mut().sync_requested = true;
                Response::from_json(&serde_json::json!({"requested": true}))
            }
            (Method::Put, api::IMPORT) => self.api_import(&mut req).await,
            _ => Ok(Response::empty()?.with_status(404)),
        };
        let wanted = std::mem::take(&mut self.core.inner.borrow_mut().sync_requested);
        if wanted {
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

    fn api_state(&self, req: &Request) -> Result<Response> {
        let etag = self.core.etag();
        let headers = Headers::new();
        headers.set("ETag", &etag)?;
        headers.set("Cache-Control", "no-store")?;
        if req.headers().get("If-None-Match")?.as_deref() == Some(etag.as_str()) {
            return Ok(Response::empty()?.with_status(304).with_headers(headers));
        }
        let state = self.core.api_state();
        Ok(Response::from_json(&state)?.with_headers(headers))
    }

    async fn api_ops(&self, req: &mut Request) -> Result<Response> {
        let batch: OpBatch = match serde_json::from_str(&req.text().await?) {
            Ok(b) => b,
            Err(e) => return Response::error(format!("bad ops: {e}"), 400),
        };
        // A resend after a lost answer: answer again, apply nothing.
        if let Some(results) = batch_answer(&self.core.sql, &batch.id)? {
            log::info!("batch {} again, answered from the record", batch.id);
            return Ok(Response::ok(results)?.with_headers(json_headers()?));
        }
        let results: Vec<OpResult> = batch
            .ops
            .into_iter()
            .map(|op| self.core.apply(op))
            .collect();
        let text = serde_json::to_string(&results).unwrap_or_default();
        let now = Date::now().as_millis() as i64;
        self.core.sql.exec(
            "INSERT OR REPLACE INTO batches (id, value, at) VALUES (?, ?, ?)",
            vec![batch.id.into(), text.as_str().into(), now.into()],
        )?;
        self.core.sql.exec(
            "DELETE FROM batches WHERE at < ?",
            vec![(now - BATCH_ANSWER_MS).into()],
        )?;
        Ok(Response::ok(text)?.with_headers(json_headers()?))
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
        if !replace && !self.core.inner.borrow().store.tasks().is_empty() {
            return Response::error("the Worker already has tasks", 409);
        }
        let n = snap.tasks.len();
        self.core.inner.borrow_mut().store = Store::from_snapshot(snap);
        self.core.persist();
        log::info!("imported {n} tasks");
        Response::from_json(&serde_json::json!({"imported": n}))
    }
}

/// The task list and what goes with it, shared by the object's handlers.
/// Nothing borrows `inner` across an `.await`, so requests that arrive
/// while a sync waits on GitHub are served in the meantime.
struct Core {
    sql: SqlStorage,
    inner: RefCell<Inner>,
    forges: RefCell<Option<Forges>>,
}

struct Inner {
    store: Store,
    /// Each task as last written, to write only the rows that changed.
    saved: HashMap<String, String>,
    saved_next_id: u64,
    /// When this instance started; with `version`, the state's ETag.
    boot: u64,
    version: u64,
    sync: SyncStatus,
    notices: VecDeque<RemoteNotice>,
    next_seq: u64,
    sync_requested: bool,
    round: usize,
}

#[derive(serde::Deserialize)]
struct Row {
    id: String,
    json: String,
}

#[derive(serde::Deserialize)]
struct MetaRow {
    value: String,
}

impl Core {
    fn load(sql: SqlStorage) -> Result<Self> {
        sql.exec(
            "CREATE TABLE IF NOT EXISTS tasks (id TEXT PRIMARY KEY, json TEXT NOT NULL)",
            None,
        )?;
        sql.exec(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            None,
        )?;
        sql.exec(
            "CREATE TABLE IF NOT EXISTS batches \
             (id TEXT PRIMARY KEY, value TEXT NOT NULL, at INTEGER NOT NULL)",
            None,
        )?;
        let rows: Vec<Row> = sql.exec("SELECT id, json FROM tasks", None)?.to_array()?;
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
        let next_manual_id = meta(&sql, "next_manual_id")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        // Kept across restarts, so a fresh instance still knows when the
        // last pass ran and how it went.
        let sync: SyncStatus = meta(&sql, "sync")?
            .and_then(|v| serde_json::from_str(&v).ok())
            .map(|s: SyncStatus| SyncStatus {
                in_progress: false,
                ..s
            })
            .unwrap_or_default();
        let store = Store::from_snapshot(Snapshot {
            tasks,
            next_manual_id,
        });
        Ok(Core {
            sql,
            inner: RefCell::new(Inner {
                store,
                saved,
                saved_next_id: next_manual_id,
                boot: Date::now().as_millis(),
                version: 0,
                sync,
                notices: VecDeque::new(),
                next_seq: 0,
                sync_requested: false,
                round: 0,
            }),
            forges: RefCell::new(None),
        })
    }

    /// Write the rows that changed since the last write.
    fn persist(&self) {
        let mut inner = self.inner.borrow_mut();
        let Inner {
            store,
            saved,
            saved_next_id,
            version,
            ..
        } = &mut *inner;
        let mut writes = 0;
        let mut present = HashSet::with_capacity(store.tasks().len());
        for t in store.tasks() {
            present.insert(t.id.as_str());
            let Ok(json) = serde_json::to_string(t) else {
                continue;
            };
            if saved.get(&t.id) == Some(&json) {
                continue;
            }
            match self.sql.exec(
                "INSERT OR REPLACE INTO tasks (id, json) VALUES (?, ?)",
                vec![t.id.as_str().into(), json.as_str().into()],
            ) {
                Ok(_) => {
                    saved.insert(t.id.clone(), json);
                    writes += 1;
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
            match self
                .sql
                .exec("DELETE FROM tasks WHERE id = ?", vec![id.as_str().into()])
            {
                Ok(_) => {
                    saved.remove(&id);
                    writes += 1;
                }
                Err(e) => log::error!("delete {id}: {e}"),
            }
        }
        let next_id = store.next_manual_id();
        if next_id != *saved_next_id {
            match self.sql.exec(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('next_manual_id', ?)",
                vec![next_id.to_string().into()],
            ) {
                Ok(_) => *saved_next_id = next_id,
                Err(e) => log::error!("save next_manual_id: {e}"),
            }
        }
        if writes > 0 {
            *version += 1;
        }
    }

    fn bump(&self) {
        self.inner.borrow_mut().version += 1;
    }

    fn etag(&self) -> String {
        let inner = self.inner.borrow();
        format!("\"{}-{}\"", inner.boot, inner.version)
    }

    fn api_state(&self) -> ApiState {
        let inner = self.inner.borrow();
        ApiState {
            boot: inner.boot,
            snapshot: inner.store.snapshot(),
            sync: inner.sync.clone(),
            notices: inner.notices.iter().cloned().collect(),
        }
    }

    fn apply(&self, op: Op) -> OpResult {
        let applied = op.apply(&mut self.inner.borrow_mut().store)?;
        if applied.changed {
            self.persist();
        }
        Ok(applied)
    }

    fn push_notice(&self, error: bool, text: String) {
        let mut inner = self.inner.borrow_mut();
        inner.next_seq += 1;
        let seq = inner.next_seq;
        inner.notices.push_back(RemoteNotice { seq, error, text });
        while inner.notices.len() > NOTICES_KEPT {
            inner.notices.pop_front();
        }
        inner.version += 1;
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
        let round = {
            let mut inner = self.inner.borrow_mut();
            inner.sync.in_progress = true;
            inner.version += 1;
            inner.round = inner.round.wrapping_add(1);
            inner.round
        };
        let opts = SyncOptions {
            on_close: cfg.on_close,
            checks: cfg.checks,
            max_individual_checks: cfg.max_individual_checks,
        };
        let errors = sync_once(&forges, self, opts, round).await;
        let new_errors = {
            let mut inner = self.inner.borrow_mut();
            inner.sync.in_progress = false;
            inner.sync.last_finished = Some(Utc::now());
            inner.version += 1;
            if inner.sync.errors == errors {
                Vec::new()
            } else {
                inner.sync.errors = errors.clone();
                errors
            }
        };
        for e in new_errors {
            self.push_notice(true, e);
        }
        let status = serde_json::to_string(&self.inner.borrow().sync).unwrap_or_default();
        if let Err(e) = self.sql.exec(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('sync', ?)",
            vec![status.into()],
        ) {
            log::error!("save sync status: {e}");
        }
    }
}

/// A value from the `meta` table.
fn meta(sql: &SqlStorage, key: &str) -> Result<Option<String>> {
    first_value(sql.exec("SELECT value FROM meta WHERE key = ?", vec![key.into()])?)
}

/// The recorded answer to an op batch.
fn batch_answer(sql: &SqlStorage, id: &str) -> Result<Option<String>> {
    first_value(sql.exec("SELECT value FROM batches WHERE id = ?", vec![id.into()])?)
}

fn first_value(cursor: worker::SqlCursor) -> Result<Option<String>> {
    Ok(cursor
        .to_array::<MetaRow>()?
        .into_iter()
        .next()
        .map(|r| r.value))
}

fn json_headers() -> Result<Headers> {
    let h = Headers::new();
    h.set("Content-Type", "application/json")?;
    Ok(h)
}

impl SyncHost for Core {
    fn with_store<R>(&self, f: impl FnOnce(&mut Store) -> R) -> R {
        f(&mut self.inner.borrow_mut().store)
    }

    fn save(&self) {
        self.persist();
    }

    fn changed(&self) {
        self.bump();
    }

    fn notice(&self, error: bool, text: String) {
        self.push_notice(error, text);
    }
}

struct CoreRef(Rc<Core>);

impl Backend for CoreRef {
    fn read<R>(&self, f: impl FnOnce(&Store) -> R) -> R {
        f(&self.0.inner.borrow().store)
    }

    fn apply(&self, op: Op) -> std::result::Result<Applied, String> {
        self.0.apply(op)
    }

    fn request_sync(&self) -> bool {
        let mut inner = self.0.inner.borrow_mut();
        inner.sync_requested = true;
        inner.sync.in_progress
    }

    fn sync_status(&self) -> SyncStatus {
        self.0.inner.borrow().sync.clone()
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
