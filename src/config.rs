//! User configuration, read from `~/.config/lunch-tray/config.toml`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::model::OnClose;

pub const APP_ID: &str = "lunch-tray";

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_ID)
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_ID)
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn store_path() -> PathBuf {
    data_dir().join("tasks.json")
}

const TEMPLATE: &str = r#"# Lunch Tray configuration.
#
# Seconds between remote syncs.
poll_interval_secs = 120

# Where a task goes when its pull request or issue is closed or merged:
# "settle" keeps it in Settled, "archive" puts it straight in the archive.
# on_close = "settle"

# GitHub accounts. With no token, Lunch Tray runs `gh auth token`.
[[github]]
# api_url = "https://api.github.com"
# token = "ghp_..."
# token_env = "GITHUB_TOKEN"
# Search queries, see https://docs.github.com/search-github/searching-on-github/searching-issues-and-pull-requests
# queries = [
#   "is:open is:pr author:@me archived:false",
#   "is:open is:pr review-requested:@me archived:false",
#   "is:open assignee:@me archived:false",
# ]
# Repositories to ignore, as owner/repo or owner/*.
# exclude = ["someorg/noisy-repo", "archived-org/*"]

# Forgejo or Gitea instances.
# [[forgejo]]
# url = "https://codeberg.org"
# token = "..."
# token_env = "FORGEJO_TOKEN"
# Query strings for /api/v1/repos/issues/search
# queries = [
#   "type=pulls&created=true",
#   "type=pulls&review_requested=true",
#   "type=issues&assigned=true",
# ]
# exclude = ["someorg/noisy-repo"]
"#;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
    /// Where closed or merged items go: `settle` (default) or `archive`.
    #[serde(default)]
    pub on_close: OnClose,
    #[serde(default)]
    pub github: Vec<GithubAccount>,
    #[serde(default)]
    pub forgejo: Vec<ForgejoAccount>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            poll_interval_secs: default_poll(),
            on_close: OnClose::Settle,
            github: vec![GithubAccount::default()],
            forgejo: Vec::new(),
        }
    }
}

fn default_poll() -> u64 {
    120
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubAccount {
    #[serde(default = "default_github_api")]
    pub api_url: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default = "default_github_queries")]
    pub queries: Vec<String>,
    /// Repositories to ignore: `owner/repo` or `owner/*`.
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl Default for GithubAccount {
    fn default() -> Self {
        GithubAccount {
            api_url: default_github_api(),
            token: None,
            token_env: None,
            queries: default_github_queries(),
            exclude: Vec::new(),
        }
    }
}

impl GithubAccount {
    /// Host name used in task ids and labels.
    pub fn host(&self) -> String {
        let trimmed = self
            .api_url
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        let host = trimmed.split('/').next().unwrap_or(trimmed);
        if host == "api.github.com" {
            "github.com".to_string()
        } else {
            host.to_string()
        }
    }

    pub fn resolve_token(&self) -> Result<String> {
        if let Some(t) = configured_token(&self.token, &self.token_env, "GITHUB_TOKEN") {
            return Ok(t);
        }
        let out = std::process::Command::new("gh")
            .args(["auth", "token", "-h", &self.host()])
            .output()
            .context("run `gh auth token`; set `token` in config.toml or install gh")?;
        if !out.status.success() {
            anyhow::bail!(
                "gh auth token failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if token.is_empty() {
            anyhow::bail!("gh auth token returned nothing");
        }
        Ok(token)
    }
}

/// Token from the config value, then the configured env var, then the
/// conventional env var.
fn configured_token(
    token: &Option<String>,
    token_env: &Option<String>,
    fallback_env: &str,
) -> Option<String> {
    if let Some(t) = token.as_deref().filter(|t| !t.is_empty()) {
        return Some(t.to_string());
    }
    token_env
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(fallback_env))
        .filter_map(|var| std::env::var(var).ok())
        .find(|t| !t.is_empty())
}

fn default_github_api() -> String {
    "https://api.github.com".to_string()
}

fn default_github_queries() -> Vec<String> {
    vec![
        "is:open is:pr author:@me archived:false".into(),
        "is:open is:pr review-requested:@me archived:false".into(),
        "is:open assignee:@me archived:false".into(),
    ]
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgejoAccount {
    pub url: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default = "default_forgejo_queries")]
    pub queries: Vec<String>,
    /// Repositories to ignore: `owner/repo` or `owner/*`.
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl ForgejoAccount {
    pub fn host(&self) -> String {
        let trimmed = self
            .url
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        trimmed.split('/').next().unwrap_or(trimmed).to_string()
    }

    pub fn resolve_token(&self) -> Result<String> {
        if let Some(t) = configured_token(&self.token, &self.token_env, "FORGEJO_TOKEN") {
            return Ok(t);
        }
        anyhow::bail!(
            "no token for {}: set `token` or `token_env` in config.toml",
            self.host()
        )
    }
}

fn default_forgejo_queries() -> Vec<String> {
    vec![
        "type=pulls&created=true".into(),
        "type=pulls&review_requested=true".into(),
        "type=issues&assigned=true".into(),
    ]
}

/// True when `owner/repo` matches one of the exclude patterns. Patterns are
/// `owner/repo` or `owner/*`, compared without regard to case.
pub fn is_excluded(patterns: &[String], owner: &str, repo: &str) -> bool {
    patterns.iter().any(|pat| {
        let Some((p_owner, p_repo)) = pat.trim().split_once('/') else {
            return false;
        };
        p_owner.eq_ignore_ascii_case(owner) && (p_repo == "*" || p_repo.eq_ignore_ascii_case(repo))
    })
}

impl Config {
    /// Load the config file. When it does not exist, write a commented
    /// template and use the defaults (one GitHub account through `gh`).
    pub fn load() -> Result<Config> {
        let path = config_path();
        if !path.exists() {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, TEMPLATE)
                .with_context(|| format!("write template {}", path.display()))?;
            log::info!("wrote default config to {}", path.display());
        }
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_to_defaults() {
        let cfg: Config = toml::from_str(TEMPLATE).unwrap();
        assert_eq!(cfg.poll_interval_secs, 120);
        assert_eq!(cfg.on_close, OnClose::Settle);
        assert_eq!(cfg.github.len(), 1);
        assert_eq!(cfg.github[0].host(), "github.com");
        assert_eq!(cfg.github[0].queries, default_github_queries());
        assert!(cfg.forgejo.is_empty());
        let cfg: Config = toml::from_str("on_close = \"archive\"").unwrap();
        assert_eq!(cfg.on_close, OnClose::Archive);
    }

    #[test]
    fn exclude_patterns_match_repos_and_owners() {
        let pats = vec![
            "Acme/Widgets".to_string(),
            "noisy/*".to_string(),
            "bad".to_string(),
        ];
        assert!(is_excluded(&pats, "acme", "widgets"));
        assert!(!is_excluded(&pats, "acme", "gadgets"));
        assert!(is_excluded(&pats, "noisy", "anything"));
        assert!(!is_excluded(&pats, "quiet", "anything"));
        assert!(!is_excluded(&[], "acme", "widgets"));
    }

    #[test]
    fn hosts_are_derived_from_urls() {
        let gh = GithubAccount {
            api_url: "https://ghe.example.com/api/v3".into(),
            ..Default::default()
        };
        assert_eq!(gh.host(), "ghe.example.com");
        let fj = ForgejoAccount {
            url: "https://codeberg.org/".into(),
            token: None,
            token_env: None,
            queries: vec![],
            exclude: vec![],
        };
        assert_eq!(fj.host(), "codeberg.org");
    }
}
