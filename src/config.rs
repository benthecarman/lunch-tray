//! User configuration, read from `~/.config/lunch-tray/config.toml`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

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

/// The copy of the Worker's tasks, for a start without network.
pub fn cache_path() -> PathBuf {
    data_dir().join("remote-cache.json")
}

const TEMPLATE: &str = r#"# Lunch Tray configuration.
#
# The tasks live on a Lunch Tray Worker on Cloudflare, which also syncs with
# GitHub and serves the MCP tools. See worker/README.md to set one up, then
# fill in this section.
# [remote]
# url = "https://lunch-tray.example.workers.dev"
# token = "..."                 # the Worker's API_TOKEN
# token_env = "LUNCH_TRAY_REMOTE_TOKEN"
# Changes come down a WebSocket. Without one, the app checks every
# poll_secs = 10               # seconds while a window is open
# idle_poll_secs = 30          # and with only the tray icon
"#;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Config {
    /// The Worker that holds the tasks.
    #[serde(default)]
    pub remote: Option<RemoteConfig>,
    /// Anything else, such as the settings of the sync this app used to run
    /// itself. The Worker has its own now; these are ignored.
    #[serde(flatten)]
    pub ignored: toml::Table,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfig {
    /// The Worker's base URL.
    pub url: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_env: Option<String>,
    /// Seconds between checks for changes while a window is open, when
    /// there is no WebSocket.
    #[serde(default = "default_poll")]
    pub poll_secs: u64,
    /// Seconds between checks with only the tray icon.
    #[serde(default = "default_idle_poll")]
    pub idle_poll_secs: u64,
}

impl RemoteConfig {
    /// The token from the config, then the configured variable, then
    /// `LUNCH_TRAY_REMOTE_TOKEN`.
    pub fn resolve_token(&self) -> Result<String> {
        if let Some(t) = self.token.as_deref().filter(|t| !t.is_empty()) {
            return Ok(t.to_string());
        }
        self.token_env
            .iter()
            .map(String::as_str)
            .chain(std::iter::once("LUNCH_TRAY_REMOTE_TOKEN"))
            .filter_map(|var| std::env::var(var).ok())
            .find(|t| !t.is_empty())
            .context("no token for the Worker: set `token` or `token_env` under [remote]")
    }
}

fn default_poll() -> u64 {
    10
}

fn default_idle_poll() -> u64 {
    30
}

impl Config {
    /// Load the config file. When it does not exist, write a commented
    /// template.
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
        if !cfg.ignored.is_empty() {
            let keys: Vec<&str> = cfg.ignored.keys().map(String::as_str).collect();
            log::warn!(
                "config: ignoring {}; the Worker syncs and serves MCP now",
                keys.join(", ")
            );
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_parses_without_a_worker() {
        let cfg: Config = toml::from_str(TEMPLATE).unwrap();
        assert!(cfg.remote.is_none());
        assert!(cfg.ignored.is_empty());
    }

    #[test]
    fn a_worker_section_parses_with_defaults() {
        let cfg: Config = toml::from_str("[remote]\nurl = \"https://w.example\"").unwrap();
        let remote = cfg.remote.unwrap();
        assert_eq!((remote.poll_secs, remote.idle_poll_secs), (10, 30));
        assert!(toml::from_str::<Config>("[remote]\nurl = \"u\"\ntypo = 1").is_err());
    }

    #[test]
    fn settings_of_the_old_local_sync_are_ignored() {
        let old = r#"
poll_interval_secs = 120
on_close = "archive"
[[github]]
[[forgejo]]
url = "https://codeberg.org"
[mcp]
listen = "127.0.0.1:7428"
[remote]
url = "https://w.example"
"#;
        let cfg: Config = toml::from_str(old).unwrap();
        assert_eq!(cfg.remote.unwrap().url, "https://w.example");
        let mut keys: Vec<&str> = cfg.ignored.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            ["forgejo", "github", "mcp", "on_close", "poll_interval_secs"]
        );
    }
}
