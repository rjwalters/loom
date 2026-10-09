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
//!    (`per_page=100` **and** `--paginate`, so the rows retrieved do not depend
//!    on how many checks a repo happens to have), projecting
//!    `name`/`status`/`conclusion`/`started_at` plus the Actions run/job ids —
//!    and `total_count`, so a read short of the forge's own count fails closed
//!    instead of yielding a subset (#8987, matching #8895's shell fix).
//!
//! Plus, for the input-scoped predicate (#8919), three more:
//!
//! 4. **`P`** — `GET /repos/{nwo}/pulls/{pr}/files` (paginated).
//! 5. **`B`** — `GET /repos/{nwo}/actions/jobs/{job}/logs`, parsed by
//!    [`parse_tested_base`].
//! 6. **`D`** — `GET /repos/{nwo}/compare/{B}...{tip}`, validated by
//!    [`compare_usable`] and narrowed by [`strip_validated_restamps`].
//!
//! And, ONLY when the side in question touched `.github/workflows/ci.yml`, up
//! to two more per side for #9065's block attribution: the workflow text at
//! that side's tree (`GET /repos/{nwo}/contents/…?ref=…` — the base tip for
//! `D`, the PR head for `P`), plus, on the `P` side, a second `pulls/{pr}/files`
//! read for that one file's patch (which (4) deliberately does not carry). A
//! merge that does not touch `ci.yml` pays for none of them, and every failure
//! is [`CiScope::Unscoped`], i.e. the whole-file behaviour from before the
//! narrowing existed.
//!
//! And, ONLY when a green required context with a usable base move has no
//! built-in spec (a consumer repo, #9589), one read of the base tip's
//! `.loom/stale-check-inputs.json` — see [`super::repo_specs`]. A 404 is
//! "no declaration"; any other failure rejects it with a warning, which leaves
//! those contexts exactly as strict as before.
//!
//! Everything in (1)–(3) is fallible I/O reporting `Err(reason)`; every caller
//! outcome of an `Err` is "refuse the merge" (the guard's fail-closed
//! contract), never "skip". (4)–(6) are different: a failure there is recorded
//! as a per-context FALLBACK to the #8248 time rule, announced on stderr, so an
//! install without `Actions: read` is no worse off than before #8919.

use super::evidence::{
    compare_usable, parse_tested_base, strip_validated_restamps, to_file_set, ChangedFile,
};
use super::inputs::{BaseMove, ScopedEvidence, CI_WORKFLOW};
use super::repo_specs::{RepoSpecs, DECLARATION_PATH};
use super::workflow_scope::{self, CiScope, Workflow};
use super::CheckRun;
use chrono::{DateTime, Utc};
use std::collections::HashMap;

use crate::gh_invocation::gh_bin;

/// Run `gh api …` with the given args, returning stdout on success. `gh` is
/// the binary — a plain argument so a caller's tests can inject a stub without
/// a process-global `LOOM_GH_BIN` (which races across parallel test threads).
fn gh_api(gh: &str, args: &[&str]) -> Result<String, String> {
    let out = crate::gh_invocation::GhInvocation::new(
        crate::gh_invocation::Operation::new("merge_guard.stale_checks"),
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

/// [`fetch_required`] on its own, for a caller that needs ONLY the base
/// branch's required-context set — #9091's zero-row settle discriminator, which
/// has no use for the base tip, the check-runs rollup, or the scoped evidence
/// [`live_inputs`] also gathers.
///
/// Shared rather than reimplemented so the two guards can never disagree about
/// what a given base branch requires: `Ok(vec![])` means "provably requires
/// nothing" (including the plan-gated case, per [`is_plan_gated`]) and `Err`
/// means "could not find out", a distinction both callers fail closed on in
/// their own way.
pub fn required_contexts(nwo: &str, base_ref: &str) -> Result<(Vec<String>, Vec<String>), String> {
    fetch_required(&gh_bin(), nwo, base_ref)
}

/// [`required_contexts`], parameterized on the `gh` binary (see [`gh_api`]).
pub fn required_contexts_with(
    gh: &str,
    nwo: &str,
    base_ref: &str,
) -> Result<(Vec<String>, Vec<String>), String> {
    fetch_required(gh, nwo, base_ref)
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

/// The check-runs rollup for the PR head, projected to the guard's fields —
/// fully paginated, and failing closed on a short read (#8987).
///
/// This mirrors `forge_get_check_runs`' GitHub branch (#8895) rather than
/// inventing its own contract, because the two answer the same question about
/// the same endpoint. Both halves matter:
///
/// - **`per_page=100` + `--paginate`**: `per_page` alone only moves the cap, so
///   a repo whose head grows past 100 check-runs would hand the freshness guard
///   a silent subset. `--paginate` follows the Link header instead.
/// - **keep `total_count`**: the projection used to drop it, leaving nothing to
///   compare the row count against. With it, a read short of the forge's own
///   count is an `Err` — which every caller turns into "refuse the merge" — and
///   never a partial `Vec<CheckRun>` the guard would evaluate as if complete.
///
/// `--jq` runs once per page, so a multi-page read arrives as a stream of
/// concatenated per-page objects that must be folded here. `total_count` repeats
/// identically on every page; `max` is the conservative pick for the comparison.
pub(crate) fn fetch_check_runs(
    gh: &str,
    nwo: &str,
    head_sha: &str,
) -> Result<Vec<CheckRun>, String> {
    let out = gh_api(gh, &[
        &format!("repos/{nwo}/commits/{head_sha}/check-runs?per_page=100"),
        "--paginate",
        "--jq",
        "{total_count: (.total_count // 0), check_runs: [(.check_runs // [])[] | {name: .name, status: .status, conclusion: .conclusion, started_at: .started_at, app: .app.slug, details_url: .details_url, id: .id}]}",
    ])?;

    let mut total: u64 = 0;
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut pages = 0usize;
    for page in serde_json::Deserializer::from_str(&out).into_iter::<serde_json::Value>() {
        let page = page.map_err(|e| format!("check-runs response was not JSON: {e}"))?;
        pages += 1;
        total = total.max(
            page.get("total_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
        );
        let page_rows = page
            .get("check_runs")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "check-runs page had no check_runs array".to_string())?;
        rows.extend(page_rows.iter().cloned());
    }
    // A `gh` that exited 0 having printed nothing is a degraded read, not an
    // authoritative "this commit has no checks" — same treatment the shell
    // helper gives it, for the same reason.
    if pages == 0 {
        return Err(format!(
            "check-runs read for {head_sha} returned no pages (gh exited 0 with empty output)"
        ));
    }
    if (rows.len() as u64) < total {
        return Err(format!(
            "check-runs read for {head_sha} was SHORT: got {} of {total} check-runs; \
failing closed rather than evaluating the freshness guard on a subset (#8987)",
            rows.len()
        ));
    }

    rows.iter()
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

/// The `.github/workflows/ci.yml` entry of `pulls/{n}/files`, with its patch —
/// the `P` side of #9065's narrowing.
///
/// [`fetch_pr_files`] deliberately reads `P` by path alone (a patch per file
/// over a large PR is a lot of payload for a set that is otherwise matched by
/// name), so this is a second, narrower read issued **only** when `P` contains
/// `ci.yml` at all. `--jq` emits at most one object, because a path appears at
/// most once across the endpoint's pages; an empty answer is `Ok(None)`, which
/// leaves the side unscoped.
fn fetch_pr_ci_file(gh: &str, nwo: &str, pr: &str) -> Result<Option<ChangedFile>, String> {
    let out = gh_api(
        gh,
        &[
            &format!("repos/{nwo}/pulls/{pr}/files"),
            "--paginate",
            "--jq",
            &format!(r#".[]? | select(.filename == "{CI_WORKFLOW}") | {{status, patch}}"#),
        ],
    )?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let v: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|e| format!("the PR's ci.yml file entry was not JSON: {e}"))?;
    Ok(Some(ChangedFile {
        path: CI_WORKFLOW.to_string(),
        status: v
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        previous_filename: None,
        patch: v.get("patch").and_then(|s| s.as_str()).map(String::from),
    }))
}

/// Which components the **PR's own** `ci.yml` hunks are a global input for
/// (#9065), attributed against the PR head's workflow — the tree
/// `pulls/{n}/files`' patches diff *to*.
///
/// Costs two extra reads, and only on a PR that edits `ci.yml` at all. Every
/// failure is [`CiScope::Unscoped`]: the whole-file `G` meaning, i.e. exactly
/// the behaviour before this narrowing existed.
fn pr_ci_scope(gh: &str, nwo: &str, pr: &str, head_sha: &str, pr_files: &[ChangedFile]) -> CiScope {
    if !pr_files.iter().any(|f| f.path == CI_WORKFLOW) {
        return CiScope::Unscoped;
    }
    let Ok(Some(file)) = fetch_pr_ci_file(gh, nwo, pr) else {
        return CiScope::Unscoped;
    };
    let Ok(text) = fetch_workflow(gh, nwo, head_sha) else {
        return CiScope::Unscoped;
    };
    workflow_scope::scope_for_files(&workflow_scope::parse(&text), &[file])
}

/// One revision's `.github/workflows/ci.yml`, as raw text.
///
/// Read only when that side actually touched that path, so the ordinary merge
/// pays nothing for #9065's narrowing. The raw media type returns the file
/// body directly rather than the base64 `content` field.
fn fetch_workflow(gh: &str, nwo: &str, sha: &str) -> Result<String, String> {
    gh_api(
        gh,
        &[
            "-H",
            "Accept: application/vnd.github.raw",
            &format!("repos/{nwo}/contents/{CI_WORKFLOW}?ref={sha}"),
        ],
    )
}

/// Which components a base move's `ci.yml` hunks are a global input for
/// (#9065), memoising the tip's workflow across the (usually single) bases.
///
/// Every failure — an unreadable tip, a suppressed patch, an add/remove — is
/// [`CiScope::Unscoped`], i.e. `ci.yml` keeps its whole-file `G` meaning. The
/// narrowing can only ever be *skipped*, never mis-applied, so no notice is
/// raised: unlike the fallbacks above, this direction is a tightening.
fn ci_scope_for(
    gh: &str,
    nwo: &str,
    tip_sha: &str,
    files: &[ChangedFile],
    cache: &mut Option<Result<Workflow, String>>,
) -> CiScope {
    if !files.iter().any(|f| f.path == CI_WORKFLOW) {
        return CiScope::Unscoped;
    }
    let workflow = cache.get_or_insert_with(|| {
        fetch_workflow(gh, nwo, tip_sha).map(|text| workflow_scope::parse(&text))
    });
    match workflow {
        Ok(wf) => workflow_scope::scope_for_files(wf, files),
        Err(_) => CiScope::Unscoped,
    }
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
        pr_ci_scope: pr_ci_scope(gh, nwo, pr, head_sha, &pr_files),
        ..ScopedEvidence::default()
    };

    let mut bases: HashMap<u64, Result<String, String>> = HashMap::new();
    let mut moves: HashMap<String, Result<(super::inputs::FileSet, CiScope), String>> =
        HashMap::new();
    let mut workflow: Option<Result<Workflow, String>> = None;
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
        if !moves.contains_key(&base) {
            // Not `or_insert_with`: the ci.yml attribution needs a mutable
            // borrow of the memoised workflow, which a closure cannot hold at
            // the same time as the entry it is filling.
            let computed = match fetch_compare(gh, nwo, &base, tip_sha) {
                Err(e) => Err(format!("the base-move compare from {base} failed ({e})")),
                Ok((status, files)) => compare_usable(&status, files.len()).map(|()| {
                    let ci = ci_scope_for(gh, nwo, tip_sha, &files, &mut workflow);
                    (to_file_set(&strip_validated_restamps(&files)), ci)
                }),
            };
            moves.insert(base.clone(), computed);
        }
        let files = moves.get(&base).cloned().unwrap_or_else(|| {
            Err("the base-move compare result went missing from the cache".to_string())
        });
        match files {
            Ok((files, ci_scope)) => {
                ev.base_moves.insert(
                    ctx.clone(),
                    BaseMove {
                        tested_base: base,
                        files,
                        ci_scope,
                    },
                );
            }
            Err(why) => {
                ev.fallbacks.insert(ctx.clone(), why);
            }
        }
    }
    // The per-repo declaration (#9589) is read only when some context with a
    // usable base move has no built-in spec, so loom's own repo never pays.
    if ev
        .base_moves
        .keys()
        .any(|ctx| super::inputs::specs_for(ctx).is_none())
    {
        ev.repo_specs = RepoSpecs::from_fetch(fetch_declaration(gh, nwo, tip_sha));
    }
    (Some(ev), notices)
}

/// The base tip's [`DECLARATION_PATH`] — the BASE tip, never the PR head, so a
/// PR cannot narrow its own freshness check (#9589).
fn fetch_declaration(gh: &str, nwo: &str, tip_sha: &str) -> Result<String, String> {
    gh_api(
        gh,
        &[
            "-H",
            "Accept: application/vnd.github.raw",
            &format!("repos/{nwo}/contents/{DECLARATION_PATH}?ref={tip_sha}"),
        ],
    )
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
