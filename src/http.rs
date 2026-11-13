//! Blocking HTTP with ureq, for the link to the Worker.

use std::time::Duration;

use anyhow::{Context, Result};
use lunch_tray_core::http::{Request, Response};

/// A ureq agent that hands back every status as a response.
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
