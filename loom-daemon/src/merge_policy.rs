//! Authoritative per-repo approval/merge policy contract (#11674).
//!
//! The fleet config (tier 1, fleet-gitops; `repos` is a fleet-tier key) may
//! declare, per repo, who holds issue-approval and PR-merge authority:
//!
//! ```json
//! { "repos": [ { "repo": "owner/name",
//!                "policy": { "approval": "auto", "merge": "human" } } ] }
//! ```
//!
//! This module is the ONE definition of that contract: [`Authority`] is the
//! only enum and [`declared`] the only reader. Consumers (the `fleet.state`
//! exporter's `ChampionState.approval` / `.merge`) call it and never define
//! values of their own. A repo with no entry, or an entry whose value is not
//! `auto` / `human`, yields `None` ("unknown") — policy is never guessed.

use std::path::Path;

use serde_json::Value;

/// Dotted config path of the per-repo policy list.
pub const REPOS_KEY: &str = "repos";

/// Who holds one authority (approval or merge) for a repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authority {
    Auto,
    Human,
}

impl Authority {
    /// The wire / config spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Authority::Auto => "auto",
            Authority::Human => "human",
        }
    }

    fn parse(v: Option<&Value>) -> Option<Self> {
        match v?.as_str()? {
            "auto" => Some(Authority::Auto),
            "human" => Some(Authority::Human),
            _ => None,
        }
    }
}

/// Declared policy for one repo; each half `None` = not declared (unknown).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RepoPolicy {
    pub approval: Option<Authority>,
    pub merge: Option<Authority>,
}

/// Pure lookup of `slug` (`owner/repo`, case-insensitive) in an effective
/// config document.
#[must_use]
pub fn declared_in(effective: &Value, slug: &str) -> RepoPolicy {
    let Some(entries) = effective.get(REPOS_KEY).and_then(Value::as_array) else {
        return RepoPolicy::default();
    };
    entries
        .iter()
        .find(|e| {
            e.get("repo")
                .and_then(Value::as_str)
                .is_some_and(|r| r.eq_ignore_ascii_case(slug))
        })
        .and_then(|e| e.get("policy"))
        .map(|p| RepoPolicy {
            approval: Authority::parse(p.get("approval")),
            merge: Authority::parse(p.get("merge")),
        })
        .unwrap_or_default()
}

/// Policy declared for `slug` in `root`'s effective (tier 1-3) config.
#[must_use]
pub fn declared(root: &Path, slug: &str) -> RepoPolicy {
    declared_in(&crate::config_resolver::resolve_effective_config(root), slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn declared_policy_maps_to_auto_and_human() {
        let cfg = json!({"repos": [{"repo": "Acme/Repo", "policy": {"approval": "auto", "merge": "human"}}]});
        let p = declared_in(&cfg, "acme/repo");
        assert_eq!(p.approval, Some(Authority::Auto));
        assert_eq!(p.merge, Some(Authority::Human));
    }

    #[test]
    fn no_entry_or_bad_value_is_unknown() {
        let cfg = json!({"repos": [{"repo": "acme/a", "policy": {"approval": "maybe"}}]});
        assert_eq!(declared_in(&cfg, "acme/other"), RepoPolicy::default());
        assert_eq!(declared_in(&cfg, "acme/a"), RepoPolicy::default());
        assert_eq!(declared_in(&json!({}), "acme/a"), RepoPolicy::default());
    }
}
