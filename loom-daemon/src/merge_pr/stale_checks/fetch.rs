//! The live-forge inputs for the required-check freshness guard (#8248).
//!
//! Three read-only `gh` calls, REST-first exactly where the shell's
//! `forge-helpers.sh` already treads, so the two implementations agree on API
//! shape:
//!
//! 1. **base tip** — `GET /repos/{nwo}/commits/{base_ref}`: the tip SHA and
//!    its *committer* date (when the commit landed on the branch; the author
//!    date of a squash merge is also the merge time, but a rebase merge's is
//!    not, and committer date is the honest "when did this tip appear").
//! 2. **required contexts** — the same TWO-source union
//!    `forge_get_required_status_check_contexts` builds (#8103): rulesets
//!    (`GET /repos/{nwo}/rules/branches/{branch}`) and classic branch
//!    protection (GraphQL `branchProtectionRule.requiredStatusCheckContexts`),
//!    because a required check configured in either is invisible to the
//!    other's API. A failure on EITHER source fails the whole lookup closed —
//!    a surviving source's answer is a partial view, and a partial view of
//!    what is required is not a safe input to a merge decision.
//! 3. **check runs** — `GET /repos/{nwo}/commits/{sha}/check-runs`
//!    (`per_page=100`, which covers this repo's largest rollup), projecting
//!    `name`/`status`/`conclusion`/`started_at`.
//!
//! Everything here is fallible I/O reporting `Err(reason)`; every caller
//! outcome of an `Err` is "refuse the merge" (the guard's fail-closed
//! contract), never "skip".

use super::CheckRun;
use chrono::{DateTime, Utc};
use std::process::Command;

/// The `gh` binary, honoring `LOOM_GH_BIN` for overrides/tests — the same
/// seam `forge_cmd::gh_bin` provides.
fn gh_bin() -> String {
    std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string())
}

/// Run `gh api …` with the given args, returning stdout on success.
fn gh_api(args: &[&str]) -> Result<String, String> {
    let out = Command::new(gh_bin())
        .arg("api")
        .args(args)
        .output()
        .map_err(|e| format!("could not exec gh api: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let trimmed = stderr.trim();
        let hint = if trimmed.is_empty() {
            format!("exit {}", out.status)
        } else {
            trimmed.to_string()
        };
        return Err(format!("gh api {} failed: {hint}", args.first().unwrap_or(&"")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What the decision needs from the forge.
pub struct LiveInputs {
    pub tip_sha: String,
    pub base_tip: DateTime<Utc>,
    pub required: Vec<String>,
    pub runs: Vec<CheckRun>,
}

fn parse_iso(label: &str, raw: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(raw)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| format!("{label} is not a parseable timestamp ({raw:?}): {e}"))
}

/// Split `owner/repo`, rejecting anything malformed — the nwo comes from the
/// caller's already-validated `$REPO_NWO`, so a malformed value is a caller
/// bug, not a forge condition.
fn split_nwo(nwo: &str) -> Result<(&str, &str), String> {
    let (owner, repo) = nwo
        .split_once('/')
        .ok_or_else(|| format!("repository must be owner/repo, got {nwo:?}"))?;
    if owner.is_empty() || repo.is_empty() {
        return Err(format!("repository must be owner/repo, got {nwo:?}"));
    }
    Ok((owner, repo))
}

/// Resolve the base branch's current tip: its SHA and commit time.
fn fetch_base_tip(nwo: &str, base_ref: &str) -> Result<(String, DateTime<Utc>), String> {
    let out = gh_api(&[
        &format!("repos/{nwo}/commits/{base_ref}"),
        "--jq",
        "[.sha, .commit.committer.date] | @tsv",
    ])?;
    let mut cols = out.split('\t');
    let sha = cols.next().unwrap_or("").trim().to_string();
    let date = cols.next().unwrap_or("").trim().to_string();
    if sha.is_empty() || date.is_empty() {
        return Err(format!("base branch '{base_ref}' resolved to an empty SHA or committer date"));
    }
    Ok((sha, parse_iso("base tip commit time", &date)?))
}

/// Required status check contexts, unioned across rulesets and classic branch
/// protection (mirroring `forge_get_required_status_check_contexts`, #8103).
fn fetch_required(nwo: &str, base_ref: &str) -> Result<Vec<String>, String> {
    let (owner, repo) = split_nwo(nwo)?;

    let ruleset = gh_api(&[
        &format!("repos/{nwo}/rules/branches/{base_ref}"),
        "--jq",
        ".[]? | select(.type == \"required_status_checks\") | .parameters.required_status_checks[]?.context",
    ])
    .map_err(|e| format!("ruleset lookup failed: {e}"))?;

    let query = "query($owner: String!, $name: String!, $ref: String!) { repository(owner: $owner, name: $name) { ref(qualifiedName: $ref) { branchProtectionRule { requiredStatusCheckContexts } } } }";
    let classic = gh_api(&[
        "graphql",
        &format!("-fquery={query}"),
        &format!("-Fowner={owner}"),
        &format!("-Fname={repo}"),
        &format!("-Fref=refs/heads/{base_ref}"),
        "--jq",
        ".data.repository.ref.branchProtectionRule.requiredStatusCheckContexts // [] | .[]",
    ])
    .map_err(|e| format!("classic branch-protection lookup failed: {e}"))?;

    // Union, order-preserving, de-duplicated: a context can legitimately be
    // required by BOTH a ruleset and a classic rule.
    let mut seen: Vec<String> = Vec::new();
    for line in ruleset.lines().chain(classic.lines()) {
        let ctx = line.trim();
        if ctx.is_empty() {
            continue;
        }
        if !seen.iter().any(|s| s == ctx) {
            seen.push(ctx.to_string());
        }
    }
    Ok(seen)
}

/// The check-runs rollup for the PR head, projected to the guard's fields.
fn fetch_check_runs(nwo: &str, head_sha: &str) -> Result<Vec<CheckRun>, String> {
    let out = gh_api(&[
        &format!("repos/{nwo}/commits/{head_sha}/check-runs"),
        "--method",
        "GET",
        "-f",
        "per_page=100",
        "--jq",
        "[.check_runs[]? | {name: .name, status: .status, conclusion: .conclusion, started_at: .started_at}]",
    ])?;
    let arr: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| format!("check-runs response was not JSON: {e}"))?;
    let arr = arr
        .as_array()
        .ok_or_else(|| "check-runs response was not an array".to_string())?;
    arr.iter()
        .map(|v| {
            let started_at = match v.get("started_at").and_then(|s| s.as_str()) {
                Some(s) if !s.is_empty() => Some(parse_iso("check run started_at", s)?),
                _ => None,
            };
            Ok(CheckRun {
                name: v
                    .get("name")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string(),
                status: v
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string(),
                conclusion: v
                    .get("conclusion")
                    .and_then(|s| s.as_str())
                    .map(String::from),
                started_at,
            })
        })
        .collect()
}

/// Gather everything [`super::assess`] needs from the live forge.
pub fn live_inputs(nwo: &str, base_ref: &str, head_sha: &str) -> Result<LiveInputs, String> {
    let (tip_sha, base_tip) = fetch_base_tip(nwo, base_ref)?;
    let required = fetch_required(nwo, base_ref)?;
    let runs = fetch_check_runs(nwo, head_sha)?;
    Ok(LiveInputs {
        tip_sha,
        base_tip,
        required,
        runs,
    })
}
