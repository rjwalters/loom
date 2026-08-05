//! Thin `gh` CLI wrappers shared by `clean.rs` / `aggressive.rs` /
//! `orphan_recovery.rs`.
//!
//! Mirrors the small slice of `loom_tools.common.github` (`gh_list`,
//! `gh_run`) these modules actually use. Not unit-tested directly — like
//! `claim_reconciliation::forge` and `work_finder::forge`, this is a thin
//! `Command` wrapper; the decision logic that consumes its output lives in
//! pure, fully-tested functions elsewhere in `worktree_ops`.

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

fn gh_command(repo_root: &Path) -> Command {
    let mut cmd = Command::new("gh");
    cmd.current_dir(repo_root);
    // #5401/#5431: cross-owner managed repo -> its own owner's installation-token
    // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner). This is the
    // single choke point every helper in this module builds its `Command` through,
    // so wiring it here covers `clean.rs` / `aggressive.rs` / `orphan_recovery.rs`
    // without touching each call site individually.
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, repo_root);
    cmd
}

/// `gh issue view <N> --json state --jq .state`. Returns `"UNKNOWN"` on any
/// failure (matches `clean.py`'s `except Exception: issue_state = "UNKNOWN"`).
#[must_use]
pub fn issue_state(repo_root: &Path, issue: u32) -> String {
    let out = gh_command(repo_root)
        .args([
            "issue",
            "view",
            &issue.to_string(),
            "--json",
            "state",
            "--jq",
            ".state",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                "UNKNOWN".to_string()
            } else {
                s
            }
        }
        _ => "UNKNOWN".to_string(),
    }
}

/// `gh api repos/{owner}/{repo}/issues/<N> --jq .state`, normalized to
/// `"OPEN"` / `"CLOSED"` / `"UNKNOWN"`.
///
/// Deliberately the REST endpoint rather than [`issue_state`]'s `gh issue
/// view` (which goes through GraphQL): GraphQL quota exhaustion under
/// concurrent agents is a live failure mode in this repo, and the callers of
/// this probe are bulk hygiene passes that can issue one call per stale file
/// (#4450). REST returns lowercase states, so they are upper-cased here to
/// match [`issue_state`]'s contract.
#[must_use]
pub fn issue_state_rest(repo_root: &Path, issue: u32) -> String {
    let out = gh_command(repo_root)
        .args([
            "api",
            &format!("repos/{{owner}}/{{repo}}/issues/{issue}"),
            "--jq",
            ".state",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_uppercase();
            match s.as_str() {
                "OPEN" | "CLOSED" => s,
                _ => "UNKNOWN".to_string(),
            }
        }
        _ => "UNKNOWN".to_string(),
    }
}

#[derive(Debug, Deserialize)]
struct PrRow {
    #[allow(dead_code)]
    number: u32,
}

/// Whether `branch` has an open PR. Returns `(has_open_pr, lookup_succeeded)`
/// — mirrors `clean.py::_check_open_pr`'s fail-closed contract: a failed
/// lookup must not be silently treated as "no open PR".
#[must_use]
pub fn has_open_pr(repo_root: &Path, branch: &str) -> (bool, bool) {
    let out = gh_command(repo_root)
        .args([
            "pr", "list", "--head", branch, "--state", "open", "--json", "number", "--limit", "1",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let rows: Result<Vec<PrRow>, _> = serde_json::from_slice(&o.stdout);
            match rows {
                Ok(v) => (!v.is_empty(), true),
                Err(_) => (false, false),
            }
        }
        _ => (false, false),
    }
}

/// `gh issue edit <N> --remove-label <remove> --add-label <add>`.
pub fn edit_labels(repo_root: &Path, issue: u32, remove: &str, add: &str) -> Result<()> {
    let out = gh_command(repo_root)
        .args([
            "issue",
            "edit",
            &issue.to_string(),
            "--remove-label",
            remove,
            "--add-label",
            add,
        ])
        .output()
        .context("failed to invoke gh issue edit")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh issue edit {issue} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// `gh issue comment <N> --body <body>`.
pub fn comment(repo_root: &Path, issue: u32, body: &str) -> Result<()> {
    let out = gh_command(repo_root)
        .args(["issue", "comment", &issue.to_string(), "--body", body])
        .output()
        .context("failed to invoke gh issue comment")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh issue comment {issue} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct BuildingIssueRow {
    pub number: u32,
    #[serde(default)]
    pub title: String,
}

/// `gh issue list --label loom:building --state open --json number,title`.
pub fn list_building_issues(repo_root: &Path) -> Result<Vec<BuildingIssueRow>> {
    let out = gh_command(repo_root)
        .args([
            "issue",
            "list",
            "--label",
            "loom:building",
            "--state",
            "open",
            "--json",
            "number,title",
        ])
        .output()
        .context("failed to invoke gh issue list")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh issue list --label loom:building failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout).context("parse gh issue list JSON")
}

/// Seconds since the most recent `labeled` timeline event for `loom:building`
/// on `issue`, or `None` if it cannot be determined (API failure, no such
/// event, unparseable timestamp). Mirrors `orphan_recovery.py::_get_building_label_age`.
#[must_use]
pub fn building_label_age_seconds(repo_root: &Path, issue: u32) -> Option<i64> {
    let out = gh_command(repo_root)
        .args([
            "api",
            &format!("repos/{{owner}}/{{repo}}/issues/{issue}/events"),
            "--jq",
            r#"[.[] | select(.event == "labeled" and .label.name == "loom:building")] | last | .created_at"#,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let ts = String::from_utf8_lossy(&out.stdout)
        .trim()
        .trim_matches('"')
        .to_string();
    if ts.is_empty() || ts == "null" {
        return None;
    }
    let dt = chrono::DateTime::parse_from_rfc3339(&ts).ok()?;
    Some(
        chrono::Utc::now()
            .signed_duration_since(dt.with_timezone(&chrono::Utc))
            .num_seconds(),
    )
}

/// Whether a `## Orphan Recovery` comment was posted on `issue` within the
/// last `dedup_seconds` (dedup guard, mirrors
/// `orphan_recovery.py::_has_recent_orphan_comment`).
#[must_use]
pub fn has_recent_orphan_comment(repo_root: &Path, issue: u32, dedup_seconds: i64) -> bool {
    let out = gh_command(repo_root)
        .args([
            "issue",
            "view",
            &issue.to_string(),
            "--json",
            "comments",
            "--jq",
            r###".comments | map(select(.body | startswith("## Orphan Recovery"))) | sort_by(.createdAt) | last | .createdAt // empty"###,
        ])
        .output();
    let Ok(out) = out else { return false };
    if !out.status.success() {
        return false;
    }
    let ts = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if ts.is_empty() {
        return false;
    }
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&ts) else {
        return false;
    };
    let age = chrono::Utc::now()
        .signed_duration_since(dt.with_timezone(&chrono::Utc))
        .num_seconds();
    age < dedup_seconds
}
