//! Thin `gh` CLI wrappers shared by `clean.rs` / `aggressive.rs` /
//! `orphan_recovery.rs`.
//!
//! Mirrors the small slice of `loom_tools.common.github` (`gh_list`,
//! `gh_run`) these modules actually use. The `Command`-issuing wrappers are not
//! unit-tested directly — like `claim_reconciliation::forge` and
//! `work_finder::forge`, they are thin `Command` wrappers; the decision logic
//! that consumes their output lives in pure, fully-tested functions elsewhere
//! in `worktree_ops`. The one exception is [`parse_open_linked_pr`], which IS a
//! pure decision function (the `state == "OPEN"` closes-graph filter) and is
//! unit-tested at the bottom of this file.

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

/// The `gh` binary to invoke. Honors `LOOM_GH_BIN` (tests / overrides), the
/// same seam `forge_cmd::gh_bin`, `forge_cached_list`, and `role_collision`
/// already use — so a fixture can steer these helpers without mutating the
/// process-wide `PATH`, which races with every other concurrently-running
/// test's `Command` spawn.
fn gh_bin() -> String {
    std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string())
}

fn gh_command(repo_root: &Path) -> Command {
    let mut cmd = Command::new(gh_bin());
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

/// Three-state result of the open-linked-PR probe (Issue #4452), replacing the
/// old `Option<u32>` that conflated a *verified* "no open linked PR" with a
/// *probe failure* (missing/failed/timed-out `gh`, unresolvable repo, non-zero
/// exit, unparseable output). Distinguishing the two matters because the probe's
/// consumers have **opposite** failure stakes:
///
/// - The #4123 open-PR **dispatch guard** must fail *open* — a forge outage must
///   never wedge dispatch — so it treats both [`OpenPrProbe::NoneOpen`] and
///   [`OpenPrProbe::ProbeFailed`] as "proceed" (only a verified `Open` blocks).
/// - The #4366 **no-progress predicate** must also fail open, but in the
///   *opposite* direction: a probe failure must NOT let a benign self-skip count
///   as a failed attempt, so it counts ONLY a verified [`OpenPrProbe::NoneOpen`]
///   toward `no_progress`, treating [`OpenPrProbe::ProbeFailed`] as "unverified,
///   don't punish".
/// - Orphan recovery (#5511) must fail toward "assume alive": only a verified
///   [`OpenPrProbe::NoneOpen`] lets a `loom:building` reset proceed;
///   [`OpenPrProbe::ProbeFailed`] blocks the reset exactly like a verified
///   `Open` does.
///
/// The old `Option<u32>` collapsed `ProbeFailed` into `None`, so a PARTIAL forge
/// outage (PR probe fails while the issue probe answers OPEN) could still accrue
/// wrongful quarantine pressure via the no-progress predicate. The enum makes it
/// impossible to silently re-conflate the two at a call site.
///
/// Lives here (rather than in `sweep_registry::guards`, where it started) so the
/// `worktree_ops` family can share ONE closes-graph implementation with the
/// registry instead of maintaining a second copy of the query — see #5511.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPrProbe {
    /// Verified: at least one *open* linked PR exists (carries its number).
    Open(u32),
    /// Verified: the forge answered and there is no open linked PR.
    NoneOpen,
    /// The probe could not produce a verdict — `gh` missing/failed/timed out,
    /// repo unresolvable, non-zero exit, or unparseable output.
    ProbeFailed,
}

/// The closes-graph GraphQL document behind [`open_linked_pr_args`]. Shared by
/// `sweep_registry::guards::SweepRegistry::probe_open_linked_pr` and
/// [`probe_open_linked_pr`] so the two transports (timeout-bounded registry
/// `Command` vs. plain `worktree_ops` `Command`) cannot drift apart.
pub const OPEN_LINKED_PR_QUERY: &str = "query($owner:String!,$repo:String!,$num:Int!){\
     repository(owner:$owner,name:$repo){\
     issue(number:$num){\
     closedByPullRequestsReferences(first:20,includeClosedPrs:false){\
     nodes{ number state } } } } }";

/// `gh` arguments for the closes-graph open-linked-PR query on `issue`.
///
/// Deliberately emits the RAW GraphQL payload rather than pushing a `--jq`
/// filter onto the wire: the `state == "OPEN"` filter is load-bearing (see
/// [`parse_open_linked_pr`]) and doing it in Rust makes it unit-testable
/// without a live `gh`/`jq` (#5511).
#[must_use]
pub fn open_linked_pr_args(owner: &str, repo: &str, issue: u32) -> Vec<String> {
    vec![
        "api".to_string(),
        "graphql".to_string(),
        "-f".to_string(),
        format!("query={OPEN_LINKED_PR_QUERY}"),
        "-F".to_string(),
        format!("owner={owner}"),
        "-F".to_string(),
        format!("repo={repo}"),
        "-F".to_string(),
        format!("num={issue}"),
    ]
}

/// Classify the raw stdout of the [`open_linked_pr_args`] query.
///
/// Filtering is on the node `state == "OPEN"`, NOT on the GraphQL
/// `includeClosedPrs:false` flag alone: live testing showed a *merged* PR still
/// returns from the closes-graph even with `includeClosedPrs:false` (it comes
/// back with `state: MERGED`), so relying on the flag would false-positive
/// forever on every issue whose PR ever merged. The `state == "OPEN"` filter is
/// the load-bearing one; the flag is kept only to trim the payload. This uses
/// the forge's closes-link graph, not `Closes #N` body-parsing. GitHub-only.
///
/// Anything that is not a well-formed, complete answer — unparseable JSON, a
/// top-level GraphQL `errors` array, a missing/`null` node list, or an OPEN
/// node whose `number` will not parse — is an [`OpenPrProbe::ProbeFailed`],
/// never a verified [`OpenPrProbe::NoneOpen`].
#[must_use]
pub fn parse_open_linked_pr(stdout: &str) -> OpenPrProbe {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout) else {
        return OpenPrProbe::ProbeFailed;
    };
    // A GraphQL response can carry `errors` alongside a partial `data` — a
    // partial answer is not a verdict.
    if v.get("errors").is_some_and(|e| !e.is_null()) {
        return OpenPrProbe::ProbeFailed;
    }
    let Some(serde_json::Value::Array(nodes)) =
        v.pointer("/data/repository/issue/closedByPullRequestsReferences/nodes")
    else {
        return OpenPrProbe::ProbeFailed;
    };
    for node in nodes {
        if node.get("state").and_then(serde_json::Value::as_str) != Some("OPEN") {
            continue;
        }
        return match node
            .get("number")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
        {
            Some(pr) => OpenPrProbe::Open(pr),
            // An OPEN node we cannot name is a malformed payload, not an
            // absence — fail toward "unverified".
            None => OpenPrProbe::ProbeFailed,
        };
    }
    OpenPrProbe::NoneOpen
}

/// Resolve `(owner, repo)` for `repo_root` via `gh repo view`.
///
/// Unlike `sweep_registry::guards::SweepRegistry::resolve_owner_repo` this does
/// NOT honor the process-global `LOOM_REPO` override: the `worktree_ops`
/// callers are always scoped to a concrete `repo_root`, and a `LOOM_REPO`
/// pointing at a *different* repo would silently answer the open-PR probe from
/// the wrong closes-graph — which, for orphan recovery, is a false
/// `NoneOpen` that greenlights resetting a live claim (#5511). `None` on any
/// failure, which callers must treat as a probe failure.
#[must_use]
pub fn resolve_owner_repo(repo_root: &Path) -> Option<(String, String)> {
    let out = gh_command(repo_root)
        .args([
            "repo",
            "view",
            "--json",
            "owner,name",
            "--jq",
            r#".owner.login + "/" + .name"#,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout.trim().split_once('/').and_then(|(o, r)| {
        if o.is_empty() || r.is_empty() {
            None
        } else {
            Some((o.to_string(), r.to_string()))
        }
    })
}

/// Best-effort probe for an **open** pull request linked to `issue` via
/// GitHub's authoritative closes-graph (`closedByPullRequestsReferences`).
///
/// The `worktree_ops` counterpart of
/// `sweep_registry::guards::SweepRegistry::probe_open_linked_pr` — same query
/// ([`open_linked_pr_args`]) and same classification
/// ([`parse_open_linked_pr`]), just resolved against `repo_root` instead of a
/// registry workspace. Added for #5511, where orphan recovery reset a
/// `loom:building` issue that had a live `Closes #N` PR open because nothing on
/// that path ever asked the forge about linked PRs.
#[must_use]
pub fn probe_open_linked_pr(repo_root: &Path, issue: u32) -> OpenPrProbe {
    // Repo resolution failure is a PROBE FAILURE, not a verified absence.
    let Some((owner, repo)) = resolve_owner_repo(repo_root) else {
        return OpenPrProbe::ProbeFailed;
    };
    let out = gh_command(repo_root)
        .args(open_linked_pr_args(&owner, &repo, issue))
        .output();
    match out {
        Ok(o) if o.status.success() => parse_open_linked_pr(&String::from_utf8_lossy(&o.stdout)),
        // Spawn error or non-zero exit (rate limit, auth failure, transient
        // forge error) is a PROBE FAILURE, not a verified "no open PR".
        _ => OpenPrProbe::ProbeFailed,
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

/// Best-effort fetch of the freshest `updated_at` among `issue`'s lease-
/// record comments (`<!-- loom:lease host=... sweep=... -->`, Issue #6179,
/// consulted here per Epic #6165 Phase 2 / Issue #6286) — the fleet-scoped
/// liveness evidence `orphan_recovery::check_untracked_building` consults as
/// the final gate before flagging a `loom:building` claim orphaned.
///
/// Uses the REST comments endpoint, not `gh issue view --json comments`
/// (`--json comments` exposes `createdAt` but not `updatedAt` at all — see
/// [`has_recent_orphan_comment`] above, which only ever needs `createdAt`).
/// The lease renewal loop's idempotent PATCH
/// (`defaults/docs/lease-renewal.md`) only ever changes a comment's
/// `updated_at`, never creates a new comment, so `updated_at` is the only
/// field that can answer "how long ago was this lease last renewed".
///
/// `None` on any failure, or when `issue` has no lease comment at all — a
/// claim predating this feature, or a lease write that failed. Per
/// `defaults/docs/lease-record.md`, callers must not treat `None` as
/// evidence of anything either way.
#[must_use]
pub fn freshest_lease_updated_at(
    repo_root: &Path,
    issue: u32,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let out = gh_command(repo_root)
        .args([
            "api",
            &format!("repos/{{owner}}/{{repo}}/issues/{issue}/comments"),
            "--paginate",
            "--jq",
            &format!(
                r#"[.[] | select(.body | startswith("{}")) | .updated_at] | max // empty"#,
                crate::claim_reconciliation::LEASE_MARKER_PREFIX
            ),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    crate::claim_reconciliation::forge::parse_max_timestamp(&out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closes_graph(nodes: &str) -> String {
        format!(
            r#"{{"data":{{"repository":{{"issue":{{"closedByPullRequestsReferences":{{"nodes":[{nodes}]}}}}}}}}}}"#
        )
    }

    #[test]
    fn open_node_is_a_verified_open_pr() {
        assert_eq!(
            parse_open_linked_pr(&closes_graph(r#"{"number":5507,"state":"OPEN"}"#)),
            OpenPrProbe::Open(5507)
        );
    }

    #[test]
    fn empty_node_list_is_a_verified_absence() {
        assert_eq!(parse_open_linked_pr(&closes_graph("")), OpenPrProbe::NoneOpen);
    }

    /// The load-bearing filter: `includeClosedPrs:false` does NOT keep merged
    /// PRs out of the closes-graph, so a `MERGED` node must never read as an
    /// open PR (it would pin an already-finished issue forever).
    #[test]
    fn merged_node_does_not_count_as_open() {
        assert_eq!(
            parse_open_linked_pr(&closes_graph(r#"{"number":5507,"state":"MERGED"}"#)),
            OpenPrProbe::NoneOpen
        );
    }

    #[test]
    fn closed_node_does_not_count_as_open() {
        assert_eq!(
            parse_open_linked_pr(&closes_graph(r#"{"number":5507,"state":"CLOSED"}"#)),
            OpenPrProbe::NoneOpen
        );
    }

    #[test]
    fn open_node_wins_over_merged_siblings() {
        assert_eq!(
            parse_open_linked_pr(&closes_graph(
                r#"{"number":1,"state":"MERGED"},{"number":2,"state":"OPEN"}"#
            )),
            OpenPrProbe::Open(2)
        );
    }

    #[test]
    fn unparseable_payload_is_a_probe_failure() {
        assert_eq!(parse_open_linked_pr(""), OpenPrProbe::ProbeFailed);
        assert_eq!(parse_open_linked_pr("not json"), OpenPrProbe::ProbeFailed);
        // Truncated: right shape, missing the node list.
        assert_eq!(
            parse_open_linked_pr(r#"{"data":{"repository":{"issue":null}}}"#),
            OpenPrProbe::ProbeFailed
        );
    }

    #[test]
    fn graphql_errors_are_a_probe_failure() {
        let body = r#"{"errors":[{"message":"rate limited"}],"data":{"repository":null}}"#;
        assert_eq!(parse_open_linked_pr(body), OpenPrProbe::ProbeFailed);
    }

    #[test]
    fn open_node_with_unusable_number_is_a_probe_failure() {
        assert_eq!(
            parse_open_linked_pr(&closes_graph(r#"{"number":null,"state":"OPEN"}"#)),
            OpenPrProbe::ProbeFailed
        );
    }

    #[test]
    fn query_args_carry_the_closes_graph_query_and_variables() {
        let args = open_linked_pr_args("rjwalters", "loom", 5501);
        assert_eq!(args[0], "api");
        assert_eq!(args[1], "graphql");
        assert!(args
            .iter()
            .any(|a| a.contains("closedByPullRequestsReferences")));
        assert!(args.iter().any(|a| a == "owner=rjwalters"));
        assert!(args.iter().any(|a| a == "repo=loom"));
        assert!(args.iter().any(|a| a == "num=5501"));
    }
}
