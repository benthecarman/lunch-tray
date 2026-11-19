//! The HTTP client the sync code talks through. The desktop app sends with
//! ureq on its sync thread; the Worker sends with `fetch`.

use anyhow::Result;
use async_trait::async_trait;

#[derive(Clone)]
pub struct Request {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// A JSON body, for POST.
    pub body: Option<String>,
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    /// A header value, by case-insensitive name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Sends one request and reads the whole answer. Only transport failures
/// are errors; any HTTP status comes back as a `Response`.
#[async_trait(?Send)]
pub trait Transport {
    async fn send(&self, req: Request) -> Result<Response>;
}

/// An answer with an error status, so callers can tell a missing item or
/// a bad token from other failures.
#[derive(Debug)]
pub struct StatusError(pub u16);

impl std::fmt::Display for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "http status {}", self.0)
    }
}

impl std::error::Error for StatusError {}

impl StatusError {
    /// A failure on the way to the server, or of the server, that the
    /// next attempt is likely to get past: a timeout, an overloaded or
    /// unreachable origin, or one of the 52x statuses Cloudflare gives
    /// when its link to the origin breaks.
    pub fn is_transient(&self) -> bool {
        matches!(self.0, 408 | 499 | 500 | 502..=504 | 520..=530)
    }
}

/// The request did not get an answer at all.
#[derive(Debug)]
pub struct Unreachable;

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no answer")
    }
}

/// True when `e` comes from a failure that may pass on its own: no
/// answer, or a transient status. Bad tokens, missing items, rate limits,
/// and answers that do not decode are not.
pub fn is_transient(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Unreachable>().is_some()
        || e.downcast_ref::<StatusError>()
            .is_some_and(StatusError::is_transient)
}
