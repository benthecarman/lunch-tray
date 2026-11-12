//! The HTTP client the sync code talks through. The desktop app sends with
//! ureq on its sync thread; the Worker sends with `fetch`.

use anyhow::Result;
use async_trait::async_trait;

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
