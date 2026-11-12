//! The MCP server on a local port: the shared protocol handler from
//! `lunch_tray_core::mcp`, served with tiny_http, over the store in
//! `Shared`. A Worker serves the same tools on its own.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use lunch_tray_core::mcp::{Backend, Incoming, MAX_BODY, PATH, Reply, Server};
use lunch_tray_core::model::Store;
use lunch_tray_core::ops::{Applied, Op};
use lunch_tray_core::sync::SyncStatus;

use crate::config::McpConfig;
use crate::shared::SharedRef;

struct Local(SharedRef);

impl Backend for Local {
    fn read<R>(&self, f: impl FnOnce(&Store) -> R) -> R {
        f(&self.0.lock().unwrap().store)
    }

    fn apply(&self, op: Op) -> Result<Applied, String> {
        self.0.lock().unwrap().apply(op)
    }

    fn request_sync(&self) -> bool {
        let s = self.0.lock().unwrap();
        s.request_sync();
        s.sync.in_progress
    }

    fn sync_status(&self) -> SyncStatus {
        self.0.lock().unwrap().sync.clone()
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
    let server = Arc::new(Server::new(
        Local(shared),
        token,
        cfg.allowed_origins.clone(),
    ));
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

fn serve(server: &Server<Local>, mut rq: tiny_http::Request) {
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
