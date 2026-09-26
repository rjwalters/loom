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
//!    `name`/`status`/`conclusion`/`started_at` plus the Actions run/job ids.
//!
//! Plus, for the input-scoped predicate (#8919), three more:
//!
//! 4. **`P`** — `GET /repos/{nwo}/pulls/{pr}/files` (paginated).
//! 5. **`B`** — `GET /repos/{nwo}/actions/jobs/{job}/logs`, parsed by
//!    [`parse_tested_base`].
//! 6. **`D`** — `GET /repos/{nwo}/compare/{B}...{tip}`, validated by
//!    [`compare_usable`] and narrowed by [`strip_validated_restamps`].
//!
//! Everything in (1)–(3) is fallible I/O reporting `Err(reason)`; every caller
//! outcome of an `Err` is "refuse the merge" (the guard's fail-closed
//! contract), never "skip". (4)–(6) are different: a failure there is recorded
//! as a per-context FALLBACK to the #8248 time rule, announced on stderr, so an
//! install without `Actions: read` is no worse off than before #8919.

use super::evidence::{
    compare_usable, parse_tested_base, strip_validated_restamps, to_file_set, ChangedFile,
};
use super::inputs::{BaseMove, ScopedEvidence};
use super::CheckRun;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::process::Command;

/// The `gh` binary, honoring `LOOM_GH_BIN` for overrides/tests — the same
/// seam `forge_cmd::gh_bin` provides.
fn gh_bin() -> String {
    std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string())
}

/// Run `gh api …` with the given args, returning stdout on success. `gh` is
/// the binary — a plain argument so a caller's tests can inject a stub without
/// a process-global `LOOM_GH_BIN` (which races across parallel test threads).
fn gh_api(gh: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(gh)
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
    /// verdict — the plan gate of [`is_plan_gated`], and any per-context
    /// fallback to the #8248 time rule. A relaxation nobody can see is how a
    /// fail-open ships unnoticed.
    pub notices: Vec<String>,
    /// `P` plus per-context `B`/`D` for the input-scoped predicate (#8919), or
    /// `None` when `P` itself could not be read — in which case EVERY context
    /// falls back to the time rule.
    pub scoped: Option<ScopedEvidence>,
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
fn fetch_base_tip(gh: &str, nwo: &str, base_ref: &str) -> Result<(String, DateTime<Utc>), String> {
    let out = gh_api(
        gh,
        &[
            &format!("repos/{nwo}/commits/{base_ref}"),
            "--jq",
            "[.sha, .commit.committer.date] | @tsv",
        ],
    )?;
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
fn fetch_required(
    gh: &str,
    nwo: &str,
    base_ref: &str,
) -> Result<(Vec<String>, Vec<String>), String> {
    let (owner, repo) = split_nwo(nwo)?;
    let mut notices: Vec<String> = Vec::new();

    let ruleset = match gh_api(gh, &[
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
    let classic = match gh_api(
        gh,
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
fn fetch_check_runs(gh: &str, nwo: &str, head_sha: &str) -> Result<Vec<CheckRun>, String> {
    let out = gh_api(gh, &[
        &format!("repos/{nwo}/commits/{head_sha}/check-runs"),
        "--method",
        "GET",
        "-f",
        "per_page=100",
        "--jq",
        "[.check_runs[]? | {name: .name, status: .status, conclusion: .conclusion, started_at: .started_at, app: .app.slug, details_url: .details_url, id: .id}]",
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
                actions_run_id: actions_run_id(
                    v.get("app").and_then(|s| s.as_str()),
                    v.get("details_url").and_then(|s| s.as_str()),
                ),
                actions_job_id: actions_job_id(
                    v.get("app").and_then(|s| s.as_str()),
                    v.get("details_url").and_then(|s| s.as_str()),
                ),
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

/// One workflow job's log, as plain text. `gh api` follows GitHub's 302 to the
/// log blob, so this is a single read. Actions logs carry ANSI colour codes,
/// which `gh` refuses to print without `--allow-escape-sequences`; without it
/// every read failed and the guard silently fell back to the time rule
/// (#9057). A `gh` predating the flag rejects it, so retry without it.
fn fetch_job_log(gh: &str, nwo: &str, job_id: u64) -> Result<String, String> {
    let path = format!("repos/{nwo}/actions/jobs/{job_id}/logs");
    match gh_api(gh, &[&path, "--allow-escape-sequences"]) {
        Err(e) if crate::ci_telemetry::api::mentions_unknown_escape_flag(&e) => {
            gh_api(gh, &[&path])
        }
        other => other,
    }
}

/// `P` — the PR's own changed files, paginated, keeping removals and renames.
fn fetch_pr_files(gh: &str, nwo: &str, pr: &str) -> Result<Vec<ChangedFile>, String> {
    let out = gh_api(
        gh,
        &[
            &format!("repos/{nwo}/pulls/{pr}/files"),
            "--paginate",
            "--jq",
            r#".[]? | [.filename, .status, (.previous_filename // "")] | @tsv"#,
        ],
    )?;
    Ok(out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut cols = line.split('\t');
            let path = cols.next().unwrap_or_default().to_string();
            let status = cols.next().unwrap_or_default().to_string();
            let prev = cols.next().unwrap_or_default();
            ChangedFile {
                path,
                status,
                previous_filename: (!prev.is_empty()).then(|| prev.to_string()),
                // P is matched by path only; no patch is needed for it.
                patch: None,
            }
        })
        .collect())
}

/// `D` — `compare/B...tip`, with the compare's own `status` so
/// [`compare_usable`] can refuse a diverged or truncated answer.
fn fetch_compare(
    gh: &str,
    nwo: &str,
    base: &str,
    head: &str,
) -> Result<(String, Vec<ChangedFile>), String> {
    let out = gh_api(
        gh,
        &[
            &format!("repos/{nwo}/compare/{base}...{head}"),
            "--jq",
            "{status: .status, files: [.files[]? | {filename, status, previous_filename, patch}]}",
        ],
    )?;
    let v: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| format!("compare response was not JSON: {e}"))?;
    let status = v
        .get("status")
        .and_then(|s| s.as_str())
        .ok_or("compare response had no status")?
        .to_string();
    let files = v
        .get("files")
        .and_then(|f| f.as_array())
        .ok_or("compare response had no files array")?
        .iter()
        .map(|f| ChangedFile {
            path: f
                .get("filename")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string(),
            status: f
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string(),
            previous_filename: f
                .get("previous_filename")
                .and_then(|s| s.as_str())
                .map(String::from),
            patch: f.get("patch").and_then(|s| s.as_str()).map(String::from),
        })
        .collect();
    Ok((status, files))
}

/// Gather `P` and, per green required context, `B` and `D` (#8919).
///
/// Every per-context failure is recorded in `fallbacks` rather than propagated:
/// one context whose log is unreadable must not cost the other eighteen their
/// input-scoped verdict. A failure to read `P` DOES return `None`, because
/// without it no clause can be evaluated at all.
///
/// Reads are memoised per job id and per tested base, so the common case (all
/// 19 required contexts in one `ci.yml` run, hence one `B`) costs one compare.
fn scoped_evidence(
    gh: &str,
    nwo: &str,
    pr: &str,
    head_sha: &str,
    tip_sha: &str,
    required: &[String],
    runs: &[CheckRun],
) -> (Option<ScopedEvidence>, Vec<String>) {
    let mut notices = Vec::new();
    let pr_files = match fetch_pr_files(gh, nwo, pr) {
        Ok(f) => f,
        Err(e) => {
            notices.push(format!(
                "required-check freshness guard (#8919): could not read PR #{pr}'s changed files \
({e}), so the input-scoped predicate cannot run for any required check"
            ));
            return (None, notices);
        }
    };
    let mut ev = ScopedEvidence {
        pr_delta: to_file_set(&pr_files),
        ..ScopedEvidence::default()
    };

    let mut bases: HashMap<u64, Result<String, String>> = HashMap::new();
    let mut moves: HashMap<String, Result<super::inputs::FileSet, String>> = HashMap::new();
    let mut sorted: Vec<&String> = required.iter().collect();
    sorted.sort();
    sorted.dedup();

    for ctx in sorted {
        let Some(run) = super::latest_run(runs, ctx) else {
            continue;
        };
        if !run.is_completed_success() {
            continue;
        }
        let Some(job_id) = run.actions_job_id else {
            ev.fallbacks.insert(
                ctx.clone(),
                "it is not a GitHub Actions job, so the base it tested cannot be read from a \
workflow log"
                    .to_string(),
            );
            continue;
        };
        let base = bases
            .entry(job_id)
            .or_insert_with(|| {
                fetch_job_log(gh, nwo, job_id)
                    .map_err(|e| format!("its job log could not be read ({e})"))
                    .and_then(|log| parse_tested_base(&log, head_sha))
            })
            .clone();
        let base = match base {
            Ok(b) => b,
            Err(why) => {
                ev.fallbacks.insert(ctx.clone(), why);
                continue;
            }
        };
        let files = moves
            .entry(base.clone())
            .or_insert_with(|| match fetch_compare(gh, nwo, &base, tip_sha) {
                Err(e) => Err(format!("the base-move compare from {base} failed ({e})")),
                Ok((status, files)) => compare_usable(&status, files.len())
                    .map(|()| to_file_set(&strip_validated_restamps(&files))),
            })
            .clone();
        match files {
            Ok(files) => {
                ev.base_moves.insert(
                    ctx.clone(),
                    BaseMove {
                        tested_base: base,
                        files,
                    },
                );
            }
            Err(why) => {
                ev.fallbacks.insert(ctx.clone(), why);
            }
        }
    }
    (Some(ev), notices)
}

/// Gather everything [`super::assess_scoped`] needs from the live forge.
pub fn live_inputs(
    nwo: &str,
    pr: &str,
    base_ref: &str,
    head_sha: &str,
) -> Result<LiveInputs, String> {
    live_inputs_with(&gh_bin(), nwo, pr, base_ref, head_sha)
}

/// [`live_inputs`], parameterized on the `gh` binary (see [`gh_api`]).
pub fn live_inputs_with(
    gh: &str,
    nwo: &str,
    pr: &str,
    base_ref: &str,
    head_sha: &str,
) -> Result<LiveInputs, String> {
    let (tip_sha, base_tip) = fetch_base_tip(gh, nwo, base_ref)?;
    let (required, mut notices) = fetch_required(gh, nwo, base_ref)?;
    let runs = fetch_check_runs(gh, nwo, head_sha)?;
    let (scoped, scoped_notices) =
        scoped_evidence(gh, nwo, pr, head_sha, &tip_sha, &required, &runs);
    notices.extend(scoped_notices);
    Ok(LiveInputs {
        tip_sha,
        base_tip,
        required,
        runs,
        notices,
        scoped,
    })
}

#[cfg(test)]
mod tests;
