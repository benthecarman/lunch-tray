//! MCP server: the task list as tools, over the Streamable HTTP transport.
//!
//! Speaks the stateless 2026-07-28 revision: no handshake, no sessions, and
//! every request carries its version and capabilities in `params._meta`.
//! Clients on the older revisions (2025-03-26 to 2025-11-25) still work:
//! they start with `initialize`, and since this server never issues a
//! session id, their later requests are stateless too.
//!
//! One endpoint, `/mcp`, takes POST. Every answer is a single JSON object;
//! nothing streams. Batches are refused.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use base64::Engine;
use chrono::Utc;
use serde_json::{Map, Value, json};

use crate::config::McpConfig;
use crate::model::{Origin, RemoteKind, Rung, Store, Task, TaskState};
use crate::shared::SharedRef;

pub const PATH: &str = "/mcp";
const MODERN: &str = "2026-07-28";
/// Older revisions, newest first. They start with `initialize`.
const LEGACY: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
const MAX_BODY: u64 = 1 << 20;
const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
/// The tool list only changes with a new build.
const LIST_TTL_MS: u64 = 60 * 60 * 1000;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const HEADER_MISMATCH: i64 = -32020;
const UNSUPPORTED_VERSION: i64 = -32022;

const INSTRUCTIONS: &str = "\
Lunch Tray is a todo list that follows the user's pull requests and issues \
on GitHub and Forgejo, plus tasks the user adds by hand. Each task is in one \
section: pinned, active, settled (done for now), or archived. Remote tasks \
come and go with sync: a new open item becomes active, a closed or merged \
one settles, and new activity on a settled or archived item brings it back \
to active. A task whose item is still open cannot be deleted, because the \
next sync would bring it back; archive it instead. Task ids look like \
`github:github.com/owner/repo#123` or `manual:7`.";

/// An HTTP request, reduced to what the handler looks at.
pub struct Incoming<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
}

impl Incoming<'_> {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, PartialEq)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<Value>,
}

impl Reply {
    fn json(status: u16, body: Value) -> Self {
        Reply {
            status,
            headers: Vec::new(),
            body: Some(body),
        }
    }

    fn empty(status: u16) -> Self {
        Reply {
            status,
            headers: Vec::new(),
            body: None,
        }
    }

    fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    fn rpc_error(status: u16, id: &Value, code: i64, message: impl Into<String>) -> Self {
        Self::rpc_error_data(status, id, code, message, None)
    }

    fn rpc_error_data(
        status: u16,
        id: &Value,
        code: i64,
        message: impl Into<String>,
        data: Option<Value>,
    ) -> Self {
        let mut error = json!({"code": code, "message": message.into()});
        if let Some(d) = data {
            error["data"] = d;
        }
        Self::json(status, json!({"jsonrpc": "2.0", "id": id, "error": error}))
    }
}

pub struct Server {
    shared: SharedRef,
    token: Option<String>,
    allowed_origins: Vec<String>,
}

impl Server {
    pub fn new(shared: SharedRef, token: Option<String>, allowed_origins: Vec<String>) -> Self {
        Server {
            shared,
            token,
            allowed_origins,
        }
    }

    pub fn handle(&self, rq: &Incoming) -> Reply {
        if rq.path != PATH {
            return Reply::empty(404);
        }
        // Browsers send Origin; MCP clients do not. Refusing unknown
        // origins keeps web pages (and DNS rebinding) out.
        if let Some(origin) = rq.header("Origin")
            && !self.allowed_origins.iter().any(|o| o == origin)
        {
            return Reply::empty(403);
        }
        if let Some(token) = &self.token {
            let given = rq
                .header("Authorization")
                .and_then(|h| h.strip_prefix("Bearer "))
                .unwrap_or("");
            if !same_secret(given.trim().as_bytes(), token.as_bytes()) {
                return Reply::empty(401).with_header("WWW-Authenticate", "Bearer");
            }
        }
        if rq.method != "POST" {
            // No GET stream in 2026-07-28, and no sessions to DELETE.
            return Reply::empty(405).with_header("Allow", "POST");
        }
        let msg: Value = match serde_json::from_slice(rq.body) {
            Ok(v) => v,
            Err(e) => {
                return Reply::rpc_error(
                    400,
                    &Value::Null,
                    PARSE_ERROR,
                    format!("Parse error: {e}"),
                );
            }
        };
        let Some(obj) = msg.as_object() else {
            let what = if msg.is_array() {
                "Batches are not supported"
            } else {
                "Expected a JSON-RPC object"
            };
            return Reply::rpc_error(400, &Value::Null, INVALID_REQUEST, what);
        };
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            return Reply::rpc_error(400, &Value::Null, INVALID_REQUEST, "Expected a request");
        };
        let Some(id) = obj.get("id") else {
            // A notification. None of them need anything from this server.
            return Reply::empty(202);
        };
        let params = obj.get("params").cloned().unwrap_or(json!({}));
        let meta_version = params
            .pointer(&format!("/_meta/{}", META_VERSION.replace('/', "~1")))
            .and_then(Value::as_str);
        match meta_version {
            Some(v) => self.modern(rq, id, method, v, &params),
            None => self.legacy(rq, id, method, &params),
        }
    }

    /// The 2026-07-28 rules: version and capabilities in every request,
    /// mirrored in headers for routing.
    fn modern(
        &self,
        rq: &Incoming,
        id: &Value,
        method: &str,
        version: &str,
        params: &Value,
    ) -> Reply {
        if version != MODERN {
            return unsupported_version(id, version);
        }
        if !params["_meta"][META_CAPABILITIES].is_object() {
            return Reply::rpc_error(
                400,
                id,
                INVALID_PARAMS,
                format!("Missing _meta field {META_CAPABILITIES}"),
            );
        }
        if let Some(r) = header_mismatch(
            id,
            "MCP-Protocol-Version",
            rq.header("MCP-Protocol-Version"),
            version,
        ) {
            return r;
        }
        if let Some(r) = header_mismatch(id, "Mcp-Method", rq.header("Mcp-Method"), method) {
            return r;
        }
        if method == "tools/call" {
            let name = params["name"].as_str().unwrap_or("");
            let header = rq.header("Mcp-Name").map(decode_header_value);
            if let Some(r) = header_mismatch(id, "Mcp-Name", header.as_deref(), name) {
                return r;
            }
        }
        let result = match method {
            "server/discover" => json!({
                "supportedVersions": supported_versions(),
                "capabilities": {"tools": {}},
                "instructions": INSTRUCTIONS,
                "ttlMs": LIST_TTL_MS,
                "cacheScope": "public",
                "_meta": {META_SERVER_INFO: server_info()},
            }),
            "tools/list" => json!({
                "tools": tool_list(),
                "ttlMs": LIST_TTL_MS,
                "cacheScope": "public",
            }),
            "tools/call" => match self.call_tool(params) {
                Ok(r) => r,
                Err(e) => return Reply::rpc_error(400, id, INVALID_PARAMS, e),
            },
            _ => {
                return Reply::rpc_error(
                    404,
                    id,
                    METHOD_NOT_FOUND,
                    format!("Unknown method {method}"),
                );
            }
        };
        let mut result = result;
        result["resultType"] = json!("complete");
        Reply::json(200, json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    /// The 2025 revisions: `initialize` first, then plain requests. This
    /// server never issues a session id, so nothing is kept between them.
    fn legacy(&self, rq: &Incoming, id: &Value, method: &str, params: &Value) -> Reply {
        if method != "initialize" {
            match rq.header("MCP-Protocol-Version") {
                None => {}
                Some(MODERN) => {
                    return Reply::rpc_error(
                        400,
                        id,
                        INVALID_PARAMS,
                        format!("Missing _meta field {META_VERSION}"),
                    );
                }
                Some(v) if LEGACY.contains(&v) => {}
                Some(v) => return unsupported_version(id, v),
            }
        }
        let result = match method {
            "initialize" => {
                let asked = params["protocolVersion"].as_str().unwrap_or("");
                let version = LEGACY.iter().find(|v| **v == asked).unwrap_or(&LEGACY[0]);
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": server_info(),
                    "instructions": INSTRUCTIONS,
                })
            }
            "ping" => json!({}),
            "tools/list" => json!({"tools": tool_list()}),
            "tools/call" => match self.call_tool(params) {
                Ok(r) => r,
                Err(e) => return Reply::rpc_error(200, id, INVALID_PARAMS, e),
            },
            _ => {
                return Reply::rpc_error(
                    200,
                    id,
                    METHOD_NOT_FOUND,
                    format!("Unknown method {method}"),
                );
            }
        };
        Reply::json(200, json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    /// Run a tool. `Err` is a protocol error (no such tool); a tool that
    /// fails returns a result with `isError` set, so the model sees why.
    fn call_tool(&self, params: &Value) -> Result<Value, String> {
        let name = params["name"].as_str().unwrap_or("");
        let empty = Map::new();
        let args = params["arguments"].as_object().unwrap_or(&empty);
        let outcome = match name {
            "list_tasks" => self.list_tasks(args),
            "get_task" => self.get_task(args),
            "add_task" => self.add_task(args),
            "update_task" => self.update_task(args),
            "move_task" => self.move_task(args),
            "delete_task" => self.delete_task(args),
            "mark_seen" => self.mark_seen(args),
            "sync_now" => self.sync_now(),
            "status" => self.status(),
            _ => return Err(format!("Unknown tool: {name}")),
        };
        Ok(match outcome {
            Ok(v) => json!({
                "content": [{"type": "text", "text": v.to_string()}],
                "structuredContent": v,
                "isError": false,
            }),
            Err(e) => json!({
                "content": [{"type": "text", "text": e}],
                "isError": true,
            }),
        })
    }

    // ----- tools -------------------------------------------------------------

    fn list_tasks(&self, args: &Map<String, Value>) -> Result<Value, String> {
        let section = str_arg(args, "section")?.unwrap_or("open");
        let query = str_arg(args, "query")?.unwrap_or("").trim();
        let limit = match args.get("limit") {
            None => 50,
            Some(v) => v
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or("limit must be a positive integer")? as usize,
        };
        let s = self.shared.lock().unwrap();
        let sections = s.store.sections();
        let wanted: &[&str] = match section {
            "open" => &["pinned", "active"],
            "all" => &["pinned", "active", "settled", "archived"],
            "pinned" => &["pinned"],
            "active" => &["active"],
            "settled" => &["settled"],
            "archived" => &["archived"],
            other => return Err(format!("Unknown section {other}")),
        };
        let all = [
            ("pinned", &sections.pinned),
            ("active", &sections.active),
            ("settled", &sections.settled),
            ("archived", &sections.archived),
        ];
        let tasks: Vec<&Task> = all
            .iter()
            .filter(|(name, _)| wanted.contains(name))
            .flat_map(|(_, list)| list.iter())
            .filter(|t| query.is_empty() || t.matches(query))
            .collect();
        let total = tasks.len();
        let shown: Vec<Value> = tasks.iter().take(limit).map(|t| task_json(t)).collect();
        Ok(json!({"tasks": shown, "total": total, "truncated": total > limit}))
    }

    fn get_task(&self, args: &Map<String, Value>) -> Result<Value, String> {
        let id = id_arg(args)?;
        let s = self.shared.lock().unwrap();
        let t = find(&s.store, id)?;
        Ok(json!({"task": task_json(t)}))
    }

    fn add_task(&self, args: &Map<String, Value>) -> Result<Value, String> {
        let title = str_arg(args, "title")?.unwrap_or("").trim();
        if title.is_empty() {
            return Err("A task needs a title.".into());
        }
        let notes = str_arg(args, "notes")?.unwrap_or("");
        let link = str_arg(args, "link")?.map(str::to_string);
        let mut s = self.shared.lock().unwrap();
        let id = s.store.add_manual(title, notes, link);
        s.save_store();
        s.notify();
        let t = find(&s.store, &id)?;
        Ok(json!({"task": task_json(t)}))
    }

    fn update_task(&self, args: &Map<String, Value>) -> Result<Value, String> {
        let id = id_arg(args)?;
        let title = str_arg(args, "title")?;
        let notes = str_arg(args, "notes")?;
        let link = str_arg(args, "link")?;
        let pinned = match args.get("pinned") {
            None => None,
            Some(v) => Some(v.as_bool().ok_or("pinned must be true or false")?),
        };
        let mut s = self.shared.lock().unwrap();
        let t = find(&s.store, id)?;
        let (old_notes, old_link) = (t.notes.clone(), t.link.clone());
        let mut changed = false;
        let mut errors = Vec::new();
        let mut apply = |r: Result<bool, String>| match r {
            Ok(c) => changed |= c,
            Err(e) => errors.push(e),
        };
        if let Some(title) = title {
            apply(s.store.rename(id, title));
        }
        if notes.is_some() || link.is_some() {
            let notes = notes.unwrap_or(&old_notes);
            let link = link.map(str::to_string).or(old_link);
            apply(s.store.edit_manual(id, notes, link));
        }
        if let Some(p) = pinned {
            apply(s.store.set_pinned(id, p));
        }
        if changed {
            s.save_store();
            s.notify();
        }
        if !errors.is_empty() {
            let mut msg = errors.join(" ");
            if changed {
                msg.push_str(" The other changes were saved.");
            }
            return Err(msg);
        }
        let t = find(&s.store, id)?;
        Ok(json!({"changed": changed, "task": task_json(t)}))
    }

    fn move_task(&self, args: &Map<String, Value>) -> Result<Value, String> {
        let id = id_arg(args)?;
        let to = str_arg(args, "to")?.ok_or("Missing argument: to")?;
        let mut s = self.shared.lock().unwrap();
        find(&s.store, id)?;
        let r = match to {
            "active" => s.store.reopen(id),
            "settled" => {
                if s.store
                    .get(id)
                    .is_some_and(|t| t.state == TaskState::Archived)
                {
                    return Err("An archived task goes back to active, not to settled.".into());
                }
                s.store.settle(id)
            }
            "archived" => s.store.archive(id),
            other => return Err(format!("Unknown section {other}")),
        };
        let changed = r?;
        if changed {
            s.save_store();
            s.notify();
        }
        let t = find(&s.store, id)?;
        Ok(json!({"changed": changed, "task": task_json(t)}))
    }

    fn delete_task(&self, args: &Map<String, Value>) -> Result<Value, String> {
        let id = id_arg(args)?;
        let mut s = self.shared.lock().unwrap();
        find(&s.store, id)?;
        s.store.delete(id)?;
        s.save_store();
        s.notify();
        Ok(json!({"deleted": id}))
    }

    fn mark_seen(&self, args: &Map<String, Value>) -> Result<Value, String> {
        let id = str_arg(args, "id")?;
        let mut s = self.shared.lock().unwrap();
        let count = match id {
            Some(id) => {
                find(&s.store, id)?;
                usize::from(s.store.mark_seen(id))
            }
            None => s.store.mark_all_seen(),
        };
        if count > 0 {
            s.save_store();
            s.notify();
        }
        Ok(json!({"cleared": count}))
    }

    fn sync_now(&self) -> Result<Value, String> {
        let s = self.shared.lock().unwrap();
        s.request_sync();
        Ok(json!({"requested": true, "already_running": s.sync.in_progress}))
    }

    fn status(&self) -> Result<Value, String> {
        let s = self.shared.lock().unwrap();
        let sections = s.store.sections();
        Ok(json!({
            "counts": {
                "pinned": sections.pinned.len(),
                "active": sections.active.len(),
                "settled": sections.settled.len(),
                "archived": sections.archived.len(),
                "unseen": s.store.count_unseen(),
            },
            "sync": {
                "in_progress": s.sync.in_progress,
                "last_finished": s.sync.last_finished,
                "errors": s.sync.errors,
            },
            "now": Utc::now(),
        }))
    }
}

/// Start the server on its own thread. Binding is retried, so the app can
/// start before the network it listens on (a tailnet address, say) is up.
pub fn start(cfg: &McpConfig, shared: SharedRef) -> Result<()> {
    let token = cfg.resolve_token();
    if token.is_none() {
        log::warn!(
            "mcp: no token set; anyone who can reach {} can use it",
            cfg.listen
        );
    }
    let server = Arc::new(Server::new(shared, token, cfg.allowed_origins.clone()));
    let listen = cfg.listen.clone();
    std::thread::Builder::new()
        .name("mcp".into())
        .spawn(move || {
            let mut warned = false;
            let http = loop {
                match tiny_http::Server::http(&listen) {
                    Ok(h) => break h,
                    Err(e) => {
                        if !warned {
                            log::warn!("mcp: cannot listen on {listen} yet ({e}); retrying");
                            warned = true;
                        }
                        std::thread::sleep(Duration::from_secs(10));
                    }
                }
            };
            log::info!("mcp: listening on http://{listen}{PATH}");
            for rq in http.incoming_requests() {
                let server = server.clone();
                // A slow client must not hold up the others.
                let spawned = std::thread::Builder::new()
                    .name("mcp-request".into())
                    .spawn(move || serve(&server, rq));
                if let Err(e) = spawned {
                    log::warn!("mcp: cannot spawn a request thread: {e}");
                }
            }
        })?;
    Ok(())
}

fn serve(server: &Server, mut rq: tiny_http::Request) {
    let headers: Vec<(String, String)> = rq
        .headers()
        .iter()
        .map(|h| (h.field.as_str().to_string(), h.value.as_str().to_string()))
        .collect();
    let method = rq.method().as_str().to_string();
    let url = rq.url().to_string();
    let path = url.split('?').next().unwrap_or("");
    let mut body = Vec::new();
    let reply = match rq.as_reader().take(MAX_BODY + 1).read_to_end(&mut body) {
        Err(e) => {
            log::debug!("mcp: reading request body: {e}");
            Reply::empty(400)
        }
        Ok(_) if body.len() as u64 > MAX_BODY => Reply::empty(413),
        Ok(_) => server.handle(&Incoming {
            method: &method,
            path,
            headers: &headers,
            body: &body,
        }),
    };
    log::debug!("mcp: {method} {path} -> {}", reply.status);
    let data = reply
        .body
        .as_ref()
        .map(|b| b.to_string())
        .unwrap_or_default();
    let mut resp = tiny_http::Response::from_string(data).with_status_code(reply.status);
    if reply.body.is_some() {
        resp.add_header(header("Content-Type", "application/json"));
    }
    for (k, v) in &reply.headers {
        resp.add_header(header(k, v));
    }
    if let Err(e) = rq.respond(resp) {
        log::debug!("mcp: writing response: {e}");
    }
}

fn header(k: &str, v: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("valid header")
}

// ----- protocol helpers ------------------------------------------------------

fn supported_versions() -> Vec<&'static str> {
    std::iter::once(MODERN)
        .chain(LEGACY.iter().copied())
        .collect()
}

fn server_info() -> Value {
    json!({
        "name": crate::config::APP_ID,
        "title": "Lunch Tray",
        "version": env!("CARGO_PKG_VERSION"),
    })
}

fn unsupported_version(id: &Value, requested: &str) -> Reply {
    Reply::rpc_error_data(
        400,
        id,
        UNSUPPORTED_VERSION,
        "Unsupported protocol version",
        Some(json!({"supported": supported_versions(), "requested": requested})),
    )
}

fn header_mismatch(id: &Value, name: &str, header: Option<&str>, body: &str) -> Option<Reply> {
    let message = match header {
        None => format!("Missing {name} header"),
        Some(h) if h == body => return None,
        Some(h) => {
            format!("Header mismatch: {name} header value '{h}' does not match body value '{body}'")
        }
    };
    Some(Reply::rpc_error(400, id, HEADER_MISMATCH, message))
}

/// Clients send header values that are not plain ASCII as
/// `=?base64?...?=`.
fn decode_header_value(v: &str) -> String {
    v.strip_prefix("=?base64?")
        .and_then(|rest| rest.strip_suffix("?="))
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_else(|| v.to_string())
}

/// Compare without leaking where the first difference is.
fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ----- tool helpers ----------------------------------------------------------

fn str_arg<'a>(args: &'a Map<String, Value>, name: &str) -> Result<Option<&'a str>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(format!("{name} must be a string")),
    }
}

fn id_arg(args: &Map<String, Value>) -> Result<&str, String> {
    str_arg(args, "id")?.ok_or_else(|| "Missing argument: id".to_string())
}

fn find<'a>(store: &'a Store, id: &str) -> Result<&'a Task, String> {
    store.get(id).ok_or_else(|| format!("No task with id {id}"))
}

fn section_of(t: &Task) -> &'static str {
    match t.rung() {
        Rung::Archived => "archived",
        Rung::Active if t.pinned => "pinned",
        Rung::Active => "active",
        Rung::Settled => "settled",
    }
}

fn task_json(t: &Task) -> Value {
    let mut v = json!({
        "id": t.id,
        "title": t.title,
        "section": section_of(t),
        "unseen": t.unseen,
        "project": t.project,
        "url": t.url(),
        "last_activity": Store::last_activity(t),
        "created_at": t.created_at,
    });
    if !t.notes.is_empty() {
        v["notes"] = json!(t.notes);
    }
    if let Origin::Remote(r) = &t.origin {
        let mut remote = json!({
            "provider": r.provider.label(),
            "host": r.host,
            "repo": r.project(),
            "number": r.number,
            "kind": match r.kind {
                RemoteKind::PullRequest => "pull_request",
                RemoteKind::Issue => "issue",
            },
            "state": r.state,
            "author": r.author,
            "draft": r.draft,
            "in_queries": r.in_queries,
        });
        if let Some(c) = &r.checks {
            remote["checks"] = json!({
                "passed": c.passed,
                "failed": c.failed,
                "pending": c.pending,
                "total": c.total,
                "merge": c.merge,
            });
        }
        v["remote"] = remote;
    }
    v
}

fn tool_list() -> Value {
    let id = json!({"type": "string", "description": "Task id, as returned by list_tasks."});
    let read_only = json!({"readOnlyHint": true, "openWorldHint": false});
    json!([
        {
            "name": "list_tasks",
            "title": "List tasks",
            "description": "List tasks, newest activity first. Each task has its section, \
                and remote tasks carry the pull request or issue state, CI results, and \
                merge state.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "section": {
                        "type": "string",
                        "enum": ["open", "pinned", "active", "settled", "archived", "all"],
                        "description": "Which tasks. \"open\" (the default) is pinned and active.",
                    },
                    "query": {
                        "type": "string",
                        "description": "Words that must each match the title, owner, repo, or number.",
                    },
                    "limit": {"type": "integer", "minimum": 1, "description": "At most this many. Default 50."},
                },
                "additionalProperties": false,
            },
            "annotations": read_only,
        },
        {
            "name": "get_task",
            "title": "Get a task",
            "description": "One task by id, with its notes.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": id},
                "required": ["id"],
                "additionalProperties": false,
            },
            "annotations": read_only,
        },
        {
            "name": "add_task",
            "title": "Add a task",
            "description": "Add a manual task to Active.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "notes": {"type": "string"},
                    "link": {"type": "string", "description": "A URL the task opens."},
                },
                "required": ["title"],
                "additionalProperties": false,
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false},
        },
        {
            "name": "update_task",
            "title": "Update a task",
            "description": "Change a task's title, pin, or (manual tasks only) notes and \
                link. Leave out what should stay. Only active tasks can be pinned.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": id,
                    "title": {"type": "string"},
                    "notes": {"type": "string", "description": "Replaces the notes."},
                    "link": {"type": "string", "description": "Replaces the link. Empty removes it."},
                    "pinned": {"type": "boolean"},
                },
                "required": ["id"],
                "additionalProperties": false,
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false},
        },
        {
            "name": "move_task",
            "title": "Move a task",
            "description": "Move a task to another section. \"settled\" is done for now and \
                drops the pin; \"active\" reopens; \"archived\" puts it away. A remote task \
                still comes back to active on new activity.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": id,
                    "to": {"type": "string", "enum": ["active", "settled", "archived"]},
                },
                "required": ["id", "to"],
                "additionalProperties": false,
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
        },
        {
            "name": "delete_task",
            "title": "Delete a task",
            "description": "Delete a task for good. Refused while its pull request or issue \
                is open; archive it instead.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": id},
                "required": ["id"],
                "additionalProperties": false,
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false},
        },
        {
            "name": "mark_seen",
            "title": "Mark as read",
            "description": "Clear the new-activity marker on one task, or on all tasks when \
                no id is given.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": id},
                "additionalProperties": false,
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
        },
        {
            "name": "sync_now",
            "title": "Sync now",
            "description": "Ask for a sync with GitHub and Forgejo now. It runs in the \
                background; call status to see when it finished.",
            "inputSchema": {"type": "object", "additionalProperties": false},
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true},
        },
        {
            "name": "status",
            "title": "Status",
            "description": "Task counts per section and the state of the last sync.",
            "inputSchema": {"type": "object", "additionalProperties": false},
            "annotations": read_only,
        },
    ])
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc::channel;

    use super::*;
    use crate::shared::Shared;

    fn server(token: Option<&str>) -> Server {
        let (main_tx, _) = channel();
        let (sync_tx, _) = channel();
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
        }));
        Server::new(
            shared,
            token.map(str::to_string),
            vec!["https://ok.example".into()],
        )
    }

    fn post(s: &Server, headers: &[(&str, &str)], body: Value) -> Reply {
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let body = body.to_string();
        s.handle(&Incoming {
            method: "POST",
            path: PATH,
            headers: &headers,
            body: body.as_bytes(),
        })
    }

    fn meta() -> Value {
        json!({META_VERSION: MODERN, META_CAPABILITIES: {}})
    }

    /// A 2026-07-28 request with matching headers.
    fn modern(s: &Server, method: &str, mut params: Value) -> Reply {
        params["_meta"] = meta();
        let name = params["name"].as_str().unwrap_or("").to_string();
        let mut headers = vec![("MCP-Protocol-Version", MODERN), ("Mcp-Method", method)];
        if method == "tools/call" {
            headers.push(("Mcp-Name", &name));
        }
        post(
            s,
            &headers,
            json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}),
        )
    }

    fn call(s: &Server, tool: &str, args: Value) -> Value {
        let r = modern(s, "tools/call", json!({"name": tool, "arguments": args}));
        assert_eq!(r.status, 200, "{r:?}");
        r.body.unwrap()["result"].clone()
    }

    fn error_code(r: &Reply) -> i64 {
        r.body.as_ref().unwrap()["error"]["code"].as_i64().unwrap()
    }

    #[test]
    fn discover_and_list_follow_2026_07_28() {
        let s = server(None);
        let r = modern(&s, "server/discover", json!({}));
        assert_eq!(r.status, 200);
        let res = &r.body.unwrap()["result"];
        assert_eq!(res["resultType"], "complete");
        assert_eq!(res["supportedVersions"][0], MODERN);
        assert!(res["capabilities"]["tools"].is_object());
        assert_eq!(res["_meta"][META_SERVER_INFO]["name"], "lunch-tray");
        assert!(res["ttlMs"].is_u64() && res["cacheScope"].is_string());

        let res = modern(&s, "tools/list", json!({})).body.unwrap()["result"].clone();
        assert!(res["ttlMs"].is_u64() && res["cacheScope"].is_string());
        let tools = res["tools"].as_array().unwrap();
        assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object"));
        assert!(tools.iter().any(|t| t["name"] == "list_tasks"));
    }

    #[test]
    fn tools_change_the_store() {
        let s = server(None);
        let res = call(
            &s,
            "add_task",
            json!({"title": "Write docs", "notes": "README"}),
        );
        assert_eq!(res["isError"], false);
        let id = res["structuredContent"]["task"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(res["content"][0]["type"], "text");

        let res = call(
            &s,
            "update_task",
            json!({"id": id, "pinned": true, "link": "https://x"}),
        );
        let task = &res["structuredContent"]["task"];
        assert_eq!(task["section"], "pinned");
        assert_eq!(task["notes"], "README");
        assert_eq!(task["url"], "https://x");

        let res = call(&s, "list_tasks", json!({"query": "docs"}));
        assert_eq!(res["structuredContent"]["total"], 1);
        let res = call(&s, "list_tasks", json!({"section": "settled"}));
        assert_eq!(res["structuredContent"]["total"], 0);

        let res = call(&s, "move_task", json!({"id": id, "to": "archived"}));
        assert_eq!(res["structuredContent"]["task"]["section"], "archived");
        let res = call(&s, "move_task", json!({"id": id, "to": "settled"}));
        assert_eq!(res["isError"], true);

        let res = call(&s, "delete_task", json!({"id": id}));
        assert_eq!(res["isError"], false);
        let res = call(&s, "get_task", json!({"id": id}));
        assert_eq!(res["isError"], true);
        assert!(
            res["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("No task")
        );
    }

    #[test]
    fn unknown_tool_is_a_protocol_error() {
        let s = server(None);
        let r = modern(&s, "tools/call", json!({"name": "nope"}));
        assert_eq!((r.status, error_code(&r)), (400, INVALID_PARAMS));
    }

    #[test]
    fn headers_must_match_the_body() {
        let s = server(None);
        let body = |method: &str| {
            json!({"jsonrpc": "2.0", "id": 1, "method": method,
                   "params": {"_meta": meta(), "name": "status"}})
        };
        let r = post(&s, &[("MCP-Protocol-Version", MODERN)], body("tools/list"));
        assert_eq!((r.status, error_code(&r)), (400, HEADER_MISMATCH));
        let r = post(
            &s,
            &[
                ("MCP-Protocol-Version", MODERN),
                ("Mcp-Method", "tools/call"),
            ],
            body("tools/call"),
        );
        assert_eq!(error_code(&r), HEADER_MISMATCH);
        let r = post(
            &s,
            &[
                ("mcp-protocol-version", MODERN),
                ("mcp-method", "tools/call"),
                ("mcp-name", "=?base64?c3RhdHVz?="),
            ],
            body("tools/call"),
        );
        assert_eq!(r.status, 200, "{r:?}");
    }

    #[test]
    fn unknown_versions_are_refused() {
        let s = server(None);
        let r = post(
            &s,
            &[
                ("MCP-Protocol-Version", "1900-01-01"),
                ("Mcp-Method", "tools/list"),
            ],
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list",
                   "params": {"_meta": {META_VERSION: "1900-01-01", META_CAPABILITIES: {}}}}),
        );
        assert_eq!((r.status, error_code(&r)), (400, UNSUPPORTED_VERSION));
        assert_eq!(r.body.unwrap()["error"]["data"]["requested"], "1900-01-01");

        let r = post(
            &s,
            &[("MCP-Protocol-Version", MODERN)],
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        );
        assert_eq!((r.status, error_code(&r)), (400, INVALID_PARAMS));
    }

    #[test]
    fn legacy_clients_initialize_then_call() {
        let s = server(None);
        let r = post(
            &s,
            &[],
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                   "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                              "clientInfo": {"name": "t", "version": "1"}}}),
        );
        let res = &r.body.unwrap()["result"];
        assert_eq!(res["protocolVersion"], "2025-06-18");
        assert!(res.get("resultType").is_none());
        assert!(
            !r.headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("Mcp-Session-Id"))
        );

        let r = post(
            &s,
            &[],
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                   "params": {"protocolVersion": "2099-01-01"}}),
        );
        assert_eq!(r.body.unwrap()["result"]["protocolVersion"], LEGACY[0]);

        let r = post(
            &s,
            &[],
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        );
        assert_eq!((r.status, r.body), (202, None));

        let r = post(
            &s,
            &[("MCP-Protocol-Version", "2025-06-18")],
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": "add_task", "arguments": {"title": "x"}}}),
        );
        assert_eq!(r.body.unwrap()["result"]["isError"], false);
        let r = post(
            &s,
            &[],
            json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}),
        );
        assert_eq!(r.body.unwrap()["result"], json!({}));
    }

    #[test]
    fn transport_rules() {
        let s = server(Some("sekrit"));
        let get = s.handle(&Incoming {
            method: "GET",
            path: PATH,
            headers: &[("Authorization".into(), "Bearer sekrit".into())],
            body: b"",
        });
        assert_eq!(get.status, 405);
        let r = post(&s, &[], json!({}));
        assert_eq!(r.status, 401);
        let r = post(&s, &[("Authorization", "Bearer wrong")], json!({}));
        assert_eq!(r.status, 401);
        let auth = ("Authorization", "Bearer sekrit");
        let r = post(&s, &[auth, ("Origin", "https://evil.example")], json!({}));
        assert_eq!(r.status, 403);
        let r = post(&s, &[auth, ("Origin", "https://ok.example")], json!([]));
        assert_eq!((r.status, error_code(&r)), (400, INVALID_REQUEST));
        let r = s.handle(&Incoming {
            method: "POST",
            path: PATH,
            headers: &[("Authorization".into(), "Bearer sekrit".into())],
            body: b"{not json",
        });
        assert_eq!(error_code(&r), PARSE_ERROR);
        let r = s.handle(&Incoming {
            method: "POST",
            path: "/other",
            headers: &[],
            body: b"",
        });
        assert_eq!(r.status, 404);
    }
}
