//! The forge reads the surviving merge-pr verbs share: a base branch's
//! required contexts and a head's check runs / workflow runs.
//!
//! These used to be the live inputs of the #8248 required-check freshness
//! guard, which #12063 slice 1 deleted along with the re-date remedy (#8508)
//! and the CI-run conclusion gate (#10444): required checks at the head are
//! GitHub's to enforce. What stays is what `required-checks`,
//! `checks-failure`, `zero-checks-settle`, `check-runs-rollup` and
//! `forge wait-checks` still read, and it goes with them in slice 2.
//!
//! 1. **required contexts** — the TWO-source union
//!    `forge_get_required_status_check_contexts` builds (#8103): rulesets
//!    (`GET /repos/{nwo}/rules/branches/{branch}`) and classic branch
//!    protection (GraphQL `branchProtectionRule.requiredStatusCheckContexts`,
//!    or the REST branch summary when GraphQL is refused, #11855). A failure
//!    on EITHER source fails the whole lookup closed, except the plan gate of
//!    [`is_plan_gated`].
//! 2. **check runs** — `GET /repos/{nwo}/commits/{sha}/check-runs`, fully
//!    paginated, failing closed on a short read (#8987).
//!
//! Cost (#11418): the rulesets read and the check runs are conditional reads
//! through the shared ETag store, every request is spaced, and a
//! secondary-limit answer holds every later request on that credential until
//! it clears ([`budget`]).

use super::CheckRun;
use chrono::{DateTime, Utc};

use crate::forge_call_stats::ForgeOp;
use crate::forge_wait_checks::reads::{GhReads, ReadError};
use crate::gh_invocation::gh_bin;
use std::path::{Path, PathBuf};

mod budget;

/// The facade operation and ledger caller of every read here.
pub const CALLER: &str = "merge_guard.stale_checks";

/// The rulesets read has no inventoried operation of its own.
const RULES_OP: ForgeOp = ForgeOp::uninventoried("branch rules read by the freshness guard");

/// Where conditional entries and memos live: the shared ETag store (none in
/// a test build that did not opt in).
fn default_store() -> Option<PathBuf> {
    crate::forge_etag_store::daemon_store_dir()
}

/// One conditional REST read through [`GhReads`] (#11418): `If-None-Match`
/// when `store` holds an entry, so an unchanged answer is a free `304`. Paced
/// and held like [`gh_api`].
fn conditional<T>(
    gh: &str,
    nwo: &str,
    store: Option<&Path>,
    read: impl FnOnce(&mut GhReads) -> Result<T, ReadError>,
) -> Result<T, String> {
    let scope = budget::scope(nwo);
    budget::before_request(&scope)?;
    let reads = GhReads::new(
        PathBuf::from(gh),
        None,
        store.map_or_else(PathBuf::new, Path::to_path_buf),
        Some(nwo),
    )?
    .with_caller(CALLER)
    .with_page_pacing(budget::pace);
    let mut reads = if store.is_some() {
        reads
    } else {
        reads.ephemeral()
    };
    let out = read(&mut reads);
    match (&out, reads.secondary_retry_at()) {
        (_, Some(at)) => budget::note_secondary(&scope, Some(at)),
        (Err(e), None) if crate::forge_call_stats::is_secondary_limit(None, e.reason()) => {
            budget::note_secondary(&scope, None);
        }
        _ => {}
    }
    out.map_err(|e| e.reason().to_string())
}

/// Run `gh api …` with the given args, returning stdout on success. `gh` is
/// the binary — a plain argument so a caller's tests can inject a stub without
/// a process-global `LOOM_GH_BIN` (which races across parallel test threads).
///
/// Paced, and refused without a request while a secondary-limit hold is in
/// force ([`budget`], #11418).
fn gh_api(gh: &str, nwo: &str, args: &[&str]) -> Result<String, String> {
    let scope = budget::scope(nwo);
    budget::before_request(&scope)?;
    let out = crate::gh_invocation::GhInvocation::new(
        crate::gh_invocation::Operation::new(CALLER),
        crate::gh_invocation::AccessIntent::Read,
        crate::gh_invocation::GhTarget::None,
        std::time::Duration::from_secs(60),
    )
    .program(gh)
    .arg("api")
    .args(args)
    .run();
    let out = match out {
        crate::cmd_out::CmdOutcome::Ran(o) => o,
        crate::cmd_out::CmdOutcome::Unavailable(u) => {
            return Err(format!("could not exec gh api: {u}"));
        }
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if crate::forge_call_stats::is_secondary_limit(None, &stderr) {
            budget::note_secondary(&scope, None);
        }
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

/// [`fetch_required`] for a caller that needs the base branch's
/// required-context set — #9091's zero-row settle discriminator, `required-checks`
/// and `forge wait-checks`.
///
/// Shared rather than reimplemented so the callers can never disagree about
/// what a given base branch requires: `Ok(vec![])` means "provably requires
/// nothing" (including the plan-gated case, per [`is_plan_gated`]) and `Err`
/// means "could not find out", a distinction both callers fail closed on in
/// their own way.
pub fn required_contexts(nwo: &str, base_ref: &str) -> Result<(Vec<String>, Vec<String>), String> {
    fetch_required(&gh_bin(), nwo, base_ref, default_store().as_deref())
}

/// [`required_contexts`], parameterized on the `gh` binary (see [`gh_api`]).
pub fn required_contexts_with(
    gh: &str,
    nwo: &str,
    base_ref: &str,
) -> Result<(Vec<String>, Vec<String>), String> {
    fetch_required(gh, nwo, base_ref, default_store().as_deref())
}

/// [`required_contexts_with`], keeping the rulesets entry in `store` (the
/// caller's ETag store; `None` = unconditional).
pub fn required_contexts_in(
    gh: &str,
    nwo: &str,
    base_ref: &str,
    store: Option<&Path>,
) -> Result<(Vec<String>, Vec<String>), String> {
    fetch_required(gh, nwo, base_ref, store)
}

/// The contexts a `rules/branches/{ref}` answer requires, in rule order.
fn ruleset_contexts(body: &str) -> Result<Vec<String>, String> {
    let v: serde_json::Value = serde_json::from_str(body.trim())
        .map_err(|e| format!("rulesets response was not JSON: {e}"))?;
    let rules = v.as_array().ok_or("rulesets response was not an array")?;
    Ok(rules
        .iter()
        .filter(|r| r.get("type").and_then(|t| t.as_str()) == Some("required_status_checks"))
        .filter_map(|r| r.pointer("/parameters/required_status_checks"))
        .filter_map(|c| c.as_array())
        .flatten()
        .filter_map(|c| c.get("context").and_then(|s| s.as_str()))
        .map(String::from)
        .collect())
}

/// The classic-protection read REST serves to any reader (#11855).
const BRANCH_OP: ForgeOp =
    ForgeOp::uninventoried("branch protection summary read by the freshness guard");

/// Classic branch protection's required contexts from the REST branch summary
/// (`GET /repos/{nwo}/branches/{branch}`, #11855), the fallback for a refused
/// GraphQL `branchProtectionRule` read. Conditional like the rulesets read.
fn classic_from_branch(
    gh: &str,
    nwo: &str,
    base_ref: &str,
    store: Option<&Path>,
) -> Result<Vec<String>, String> {
    let url = format!("repos/{nwo}/branches/{base_ref}");
    let body = conditional(gh, nwo, store, |r| r.read(&url, BRANCH_OP))?;
    branch_protection_contexts(&body)
}

/// The contexts a `branches/{ref}` answer's `protection` summary requires
/// (`contexts` plus `checks[].context`, de-duplicated). An unprotected branch
/// requires none. A protected branch whose answer carries no `protection`
/// object, or a `protection` with no `required_status_checks` object (GitHub
/// always sends one, `enforcement_level: "off"` when unused), is an `Err`:
/// the summary was withheld, not empty, and the caller fails closed on it.
pub(crate) fn branch_protection_contexts(body: &str) -> Result<Vec<String>, String> {
    let v: serde_json::Value = serde_json::from_str(body.trim())
        .map_err(|e| format!("branch response was not JSON: {e}"))?;
    let protected = v
        .get("protected")
        .and_then(serde_json::Value::as_bool)
        .ok_or("branch response has no `protected` flag")?;
    let Some(protection) = v.get("protection").filter(|p| p.is_object()) else {
        return if protected {
            Err("branch is protected but its `protection` summary was withheld".into())
        } else {
            Ok(Vec::new())
        };
    };
    let checks = Some(
        protection
            .get("required_status_checks")
            .filter(|c| c.is_object())
            .ok_or("branch protection summary has no `required_status_checks`")?,
    );
    let named = checks
        .and_then(|c| c.get("contexts"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str);
    let by_app = checks
        .and_then(|c| c.get("checks"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("context").and_then(serde_json::Value::as_str));
    let mut out: Vec<String> = Vec::new();
    for ctx in named.chain(by_app) {
        if !ctx.trim().is_empty() && !out.iter().any(|s| s == ctx) {
            out.push(ctx.to_string());
        }
    }
    Ok(out)
}

/// Required status check contexts, unioned across rulesets and classic branch
/// protection (mirroring `forge_get_required_status_check_contexts`, #8103),
/// plus any [`plan_gated_notice`] the lookup had to record.
///
/// The plan gate is evaluated PER SOURCE rather than "only when both are
/// gated": a gated source provably holds no rules of its own, and a source
/// that answered normally is still authoritative for what it does hold.
///
/// The rulesets read is conditional (#11418); the classic read is GraphQL,
/// which has no ETag, so it is sent every time.
fn fetch_required(
    gh: &str,
    nwo: &str,
    base_ref: &str,
    store: Option<&Path>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let (owner, repo) = split_nwo(nwo)?;
    let mut notices: Vec<String> = Vec::new();

    let url = format!("repos/{nwo}/rules/branches/{base_ref}");
    let ruleset = match conditional(gh, nwo, store, |r| r.read(&url, RULES_OP))
        .and_then(|body| ruleset_contexts(&body))
    {
        Ok(contexts) => contexts.join("\n"),
        Err(e) if is_plan_gated(&e) => {
            notices.push(plan_gated_notice("ruleset", base_ref, &e));
            String::new()
        }
        Err(e) => return Err(format!("ruleset lookup failed: {e}")),
    };

    let query = "query($owner: String!, $name: String!, $ref: String!) { repository(owner: $owner, name: $name) { ref(qualifiedName: $ref) { branchProtectionRule { requiredStatusCheckContexts } } } }";
    let classic = match gh_api(
        gh,
        nwo,
        &[
            "graphql",
            &format!("-fquery={query}"),
            &format!("-Fowner={owner}"),
            &format!("-Fname={repo}"),
            &format!("-Fref=refs/heads/{base_ref}"),
            "--jq",
            ".data.repository.ref.branchProtectionRule.requiredStatusCheckContexts // [] | .[]",
        ],
    ) {
        Ok(out) => out,
        // The same plan gate, reachable from the legacy source too: whichever
        // API GitHub declines on plan grounds, it is declining to serve a
        // feature the repository does not have.
        Err(e) if is_plan_gated(&e) => {
            notices.push(plan_gated_notice("classic branch-protection", base_ref, &e));
            String::new()
        }
        // #11855: the GraphQL field needs `Administration: read`, which a
        // read-only App token lacks ("Resource not accessible by
        // integration"). The REST branch summary answers the same question
        // with read access alone, so a refused GraphQL read is retried there
        // before the lookup fails closed.
        Err(e) => match classic_from_branch(gh, nwo, base_ref, store) {
            Ok(contexts) => {
                log::debug!("required contexts for {nwo}@{base_ref}: GraphQL failed ({e}); read the REST branch summary instead (#11855)");
                contexts.join("\n")
            }
            Err(rest) => {
                return Err(format!(
                    "classic branch-protection lookup failed: {e}; REST branch fallback: {rest}"
                ))
            }
        },
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

/// The check-runs rollup for the PR head, projected to the guard's fields —
/// fully paginated, and failing closed on a short read (#8987).
///
/// Read through [`GhReads::check_runs`] (#11418), the same conditional read
/// and the same ETag entry `forge wait-checks` uses for this SHA, so a guard
/// run after a wait is a free `304`. That read folds every page and returns
/// [`ReadError::Fatal`] when it holds fewer rows than the forge's own
/// `total_count`: a subset is never evaluated as if complete.
pub(crate) fn fetch_check_runs(
    gh: &str,
    nwo: &str,
    head_sha: &str,
) -> Result<Vec<CheckRun>, String> {
    let store = default_store();
    let rollup = conditional(gh, nwo, store.as_deref(), |r| r.check_runs(head_sha))
        .map_err(|e| format!("check-runs read for {head_sha} failed: {e}"))?;
    project_check_runs(&rollup, head_sha)
}

/// The workflow runs on `head_sha` (one page), through the same paced,
/// conditional read as [`fetch_check_runs`] — for `merge-pr
/// check-runs-rollup`'s superseded-twin drop (#11097).
///
/// # Errors
///
/// When the read fails; the caller then drops nothing.
pub fn fetch_workflow_runs(
    gh: &str,
    nwo: &str,
    head_sha: &str,
) -> Result<serde_json::Value, String> {
    conditional(gh, nwo, default_store().as_deref(), |r| r.workflow_runs(head_sha))
}

/// The guard's [`CheckRun`]s from a `{total_count, check_runs}` rollup.
fn project_check_runs(rollup: &serde_json::Value, head_sha: &str) -> Result<Vec<CheckRun>, String> {
    let rows = rollup
        .get("check_runs")
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("check-runs read for {head_sha} had no check_runs array"))?;
    let total = rollup
        .get("total_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if (rows.len() as u64) < total {
        return Err(format!(
            "check-runs read for {head_sha} was SHORT: got {} of {total} check-runs; \
failing closed rather than evaluating the freshness guard on a subset (#8987)",
            rows.len()
        ));
    }
    rows.iter()
        .map(|v| {
            let str_of = |k: &str| v.get(k).and_then(|s| s.as_str());
            let started_at = match str_of("started_at") {
                Some(s) if !s.is_empty() => Some(parse_iso("check run started_at", s)?),
                _ => None,
            };
            let app = v.pointer("/app/slug").and_then(|s| s.as_str());
            let details_url = str_of("details_url");
            Ok(CheckRun {
                name: str_of("name").unwrap_or_default().to_string(),
                status: str_of("status").unwrap_or_default().to_string(),
                conclusion: str_of("conclusion").map(String::from),
                started_at,
                actions_run_id: actions_run_id(app, details_url),
                actions_job_id: actions_job_id(app, details_url),
            })
        })
        .collect()
}

/// The GitHub Actions workflow run a check run belongs to, when it is an
/// Actions job. Parsed from `details_url`
/// (`https://github.com/<o>/<r>/actions/runs/<run>/job/<job>`) and only
/// trusted when the reporting app is `github-actions`: a third-party app's
/// check run has no workflow run at all, and a URL that merely looks like one
/// must not be mistaken for one. Reported for diagnostics; the tested base
/// comes from the JOB id (see [`actions_job_id`]).
#[must_use]
pub fn actions_run_id(app_slug: Option<&str>, details_url: Option<&str>) -> Option<u64> {
    if app_slug != Some("github-actions") {
        return None;
    }
    let rest = details_url?.split("/actions/runs/").nth(1)?;
    let id = rest.split('/').next()?;
    id.parse::<u64>().ok()
}

/// The GitHub Actions **job** id a check run is, parsed from the same
/// `details_url` (`…/actions/runs/<run>/job/<job>`) and trusted under the same
/// `github-actions` app condition as [`actions_run_id`]. This is the id whose
/// log carries the `Merge <head> into <B>` line (#8919).
#[must_use]
pub fn actions_job_id(app_slug: Option<&str>, details_url: Option<&str>) -> Option<u64> {
    if app_slug != Some("github-actions") {
        return None;
    }
    let rest = details_url?.split("/job/").nth(1)?;
    let id = rest.split(['/', '?', '#']).next()?;
    id.parse::<u64>().ok()
}

#[cfg(test)]
mod conditional_tests;
