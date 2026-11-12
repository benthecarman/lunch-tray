//! What the Lunch Tray app and its Cloudflare Worker share: the task model
//! and its state machine, the operations that change it, the GitHub and
//! Forgejo sync, and the MCP protocol handler.

pub mod api;
pub mod http;
pub mod mcp;
pub mod model;
pub mod ops;
pub mod sync;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
