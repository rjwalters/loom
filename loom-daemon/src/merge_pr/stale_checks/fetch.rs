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
//!    what is required is not a safe input to a merge decision. The ONE
//!    exception is the plan gate described in
//!    [`is_plan_gated`]: a source GitHub refuses to serve because the
//!    repository's plan does not include it cannot be holding rules.
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
    /// Degradations the caller must SAY OUT LOUD but that do not change the
    /// verdict — today only the plan gate of [`is_plan_gated`]. A relaxation
    /// nobody can see is how a fail-open ships unnoticed.
    pub notices: Vec<String>,
}

/// Is this `gh` failure GitHub declining to serve a branch-protection source
/// because the repository's PLAN does not include it (#8844)?
///
/// On a **private** repository owned by a Free account or org, rulesets and
/// classic branch protection are paid features, and
/// `GET /repos/{nwo}/rules/branches/{branch}` answers:
///
/// ```text
/// HTTP 403: Upgrade to GitHub Pro or make this repository public to enable this feature.
/// ```
///
/// Treating that as a lookup failure fails the whole guard closed, which on
/// such a repo blocks EVERY merge forever with no flag that helps — the state
/// #8844 reports. But the guard's question has a definite answer there: a
/// source the plan gates out cannot hold a `required_status_checks` rule, so
/// it configures **no required contexts**, exactly like the ordinary
/// "succeeded, returned nothing" case this fetch already treats as empty.
///
/// The predicate is deliberately narrow, and matches on the message GitHub
/// sends rather than the status code: 403 alone is ambiguous (a token missing
/// a scope, SSO enforcement, a rate-limit refusal and a plan gate all share
/// it), while "upgrade … / make this repository public" is emitted for exactly
/// one reason. Both fragments must be present. Anything else — network,
/// auth scope, rate limit, 404, a malformed response — still fails closed.
///
/// Matching a text signature is the whole reason it stays narrow: if GitHub
/// reworded it, this returns false and the guard fails closed again, which is
/// the safe direction to be wrong in.
#[must_use]
pub fn is_plan_gated(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("upgrade to github") && lower.contains("make this repository public")
}

/// The operator-facing note for a plan-gated source, naming the source, the
/// branch, and what the guard concluded from it.
fn plan_gated_notice(source: &str, base_ref: &str, err: &str) -> String {
    format!(
        "required-check freshness guard (#8248): the {source} lookup for '{base_ref}' is \
gated by this repository's GitHub plan ({err}). Rulesets and branch protection are \
unavailable on a private repository on that plan, so this source cannot make any check \
REQUIRED and is read as configuring none (#8844). Every other lookup failure still \
refuses the merge."
    )
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
/// protection (mirroring `forge_get_required_status_check_contexts`, #8103),
/// plus any [`plan_gated_notice`] the lookup had to record.
///
/// The plan gate is evaluated PER SOURCE rather than "only when both are
/// gated": a gated source provably holds no rules of its own, and a source
/// that answered normally is still authoritative for what it does hold.
fn fetch_required(nwo: &str, base_ref: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let (owner, repo) = split_nwo(nwo)?;
    let mut notices: Vec<String> = Vec::new();

    let ruleset = match gh_api(&[
        &format!("repos/{nwo}/rules/branches/{base_ref}"),
        "--jq",
        ".[]? | select(.type == \"required_status_checks\") | .parameters.required_status_checks[]?.context",
    ]) {
        Ok(out) => out,
        Err(e) if is_plan_gated(&e) => {
            notices.push(plan_gated_notice("ruleset", base_ref, &e));
            String::new()
        }
        Err(e) => return Err(format!("ruleset lookup failed: {e}")),
    };

    let query = "query($owner: String!, $name: String!, $ref: String!) { repository(owner: $owner, name: $name) { ref(qualifiedName: $ref) { branchProtectionRule { requiredStatusCheckContexts } } } }";
    let classic = match gh_api(&[
        "graphql",
        &format!("-fquery={query}"),
        &format!("-Fowner={owner}"),
        &format!("-Fname={repo}"),
        &format!("-Fref=refs/heads/{base_ref}"),
        "--jq",
        ".data.repository.ref.branchProtectionRule.requiredStatusCheckContexts // [] | .[]",
    ]) {
        Ok(out) => out,
        // The same plan gate, reachable from the legacy source too: whichever
        // API GitHub declines on plan grounds, it is declining to serve a
        // feature the repository does not have.
        Err(e) if is_plan_gated(&e) => {
            notices.push(plan_gated_notice("classic branch-protection", base_ref, &e));
            String::new()
        }
        Err(e) => return Err(format!("classic branch-protection lookup failed: {e}")),
    };

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
    Ok((seen, notices))
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
    let (required, notices) = fetch_required(nwo, base_ref)?;
    let runs = fetch_check_runs(nwo, head_sha)?;
    Ok(LiveInputs {
        tip_sha,
        base_tip,
        required,
        runs,
        notices,
    })
}
