//! The CI-run conclusion gate (#10444): refuse a merge whose head's latest
//! `CI` workflow run did not conclude `success`.
//!
//! # The incident
//!
//! PR #10403 merged with Rust lint and the Rust unit tests never run: a hosted
//! runner was never acquired, so `Detect Changes` was cancelled and every job
//! behind it skipped. The `main` ruleset requires only three always-run
//! contexts, all green, so the forge and `merge-pr.sh` both read the PR as
//! green. "Required contexts green" is not the same statement as "the CI run
//! succeeded"; this gate asks the second question directly, as the
//! belt-and-braces companion to ci.yml's `CI Result` aggregate job.
//!
//! # Contract
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | latest `CI` run for EXACTLY this head concluded `success` | [`CLEAN`] | 0 |
//! | the repository defines no `CI` workflow at all, or its listed `CI` workflow file is verifiably absent at both the PR's base and head (#12039) | [`NO_CI_WORKFLOW`] + reason | 0 |
//! | a `CI` workflow exists but no run for this head / run still in progress | [`UNVERIFIED`] + reason | 3 |
//! | latest run concluded anything else | the refusal (names the cancelled / failed jobs, hints `gh run rerun --failed`), ending in a [`FAILED`] line | 1 |
//! | could not query the forge (runs or workflows) | the reason | 2 |
//!
//! Only [`CLEAN`] and [`NO_CI_WORKFLOW`] permit a merge (#10567). Every other
//! outcome is a refusal or an unknown, and an unknown HOLDS the merge: the
//! caller (`merge-pr.sh`'s `_check_ci_result`) re-queues on exit 3 and refuses
//! on exit 2, an older binary, or any unexpected output. Before #10567 the
//! unknowns exited 0 and the caller warned and merged anyway — an unavailable
//! verifier was treated as permission to continue.
//!
//! "No `CI` workflow" is the legitimate no-CI policy, answered from the
//! repository's own workflow list (`GET actions/workflows`), and is kept
//! separate from "cannot inspect": a workflow-list read that fails or is
//! truncated is exit 2, never a no-CI pass. A run counts only when its
//! `head_sha` is exactly the requested head, so a run for an older head (the
//! branch moved) is not evidence about this one.
//!
//! # A listed workflow is not necessarily one the PR runs (#12039)
//!
//! `GET actions/workflows` also lists workflows registered only from a
//! non-default branch: a `ci.yml` pushed on a feature branch that never
//! reached `main` stays listed as an active `CI` workflow forever. A PR off
//! `main` never contains that file, so no `CI` run ever starts and the gate
//! held such PRs UNVERIFIED on every pass. So a listed `CI` workflow counts
//! only when its `path` exists at the PR's base ref or at its head
//! ([`workflow_applies`]). Only a verified absence at BOTH refs (a contents
//! read answering `Not Found`) drops it; any other read failure, or a record
//! with no `path`, keeps today's behaviour (the workflow applies, so a missing
//! run is UNVERIFIED and the merge holds).
//!
//! The remedy is `gh run rerun --failed <run>`: it re-runs in place, so no
//! distinct commit's run is ever cancelled (ci-principles.md).

use serde_json::Value;

/// Stdout of a clean verdict.
pub const CLEAN: &str = "LOOM-CI-RESULT-CLEAN";
/// Stdout prefix when no definite verdict exists yet (exit 3; callers HOLD).
pub const UNVERIFIED: &str = "LOOM-CI-RESULT-UNVERIFIED";
/// Stdout prefix when the repository defines no `CI` workflow (exit 0): the
/// explicit no-CI policy, distinct from an inability to inspect CI.
pub const NO_CI_WORKFLOW: &str = "LOOM-CI-RESULT-NO-CI-WORKFLOW";

/// Prefix of the refusal's last line, `LOOM-CI-RESULT-FAILED run=<id>
/// jobs=<names>` (#11330): a stable handle a driver can parse to decide on
/// `gh run rerun --failed <id>`. Classification only — this gate never decides
/// "flake vs. real" and never re-runs anything itself.
pub const FAILED: &str = "LOOM-CI-RESULT-FAILED";

/// `LOOM-CI-RESULT-FAILED run=<id> jobs=<names>`: `names` are the bad jobs'
/// names, comma-separated, or `-` when none could be listed. Last on the line
/// because a job name may contain spaces.
#[must_use]
pub fn failed_line(run: u64, jobs: &[String]) -> String {
    let names: Vec<&str> = jobs
        .iter()
        .map(|j| j.rsplit_once(" (").map_or(j.as_str(), |(n, _)| n))
        .collect();
    let names = if names.is_empty() {
        "-".to_string()
    } else {
        names.join(",")
    };
    format!("{FAILED} run={run} jobs={names}")
}

/// The workflow whose conclusion is gated: the `name:` of ci.yml.
pub const WORKFLOW_NAME: &str = "CI";

/// A job conclusion that makes a run non-green.
const BAD_JOB: &[&str] = &[
    "cancelled",
    "failure",
    "timed_out",
    "startup_failure",
    "action_required",
];

/// What the gate decided.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Clean,
    /// The repository defines no `CI` workflow: nothing to gate on.
    NoCiWorkflow(String),
    /// A `CI` workflow exists but has no concluded run for this head yet.
    Unverified(String),
    Refuse(String),
    /// The forge could not be read (or answered incompletely).
    Unreadable(String),
}

/// The newest `CI` run for exactly head `sha` in a `GET actions/runs?head_sha=`
/// payload: `(id, status, conclusion, html_url)`. A run whose `head_sha` is
/// missing or names another commit is ignored (#10567: exact-head semantics).
pub fn latest_run(runs: &Value, sha: &str) -> Option<(u64, String, String, String)> {
    let arr = runs.get("workflow_runs")?.as_array()?;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    arr.iter()
        .filter(|r| r.get("name").and_then(Value::as_str) == Some(WORKFLOW_NAME))
        .filter(|r| !sha.is_empty() && r.get("head_sha").and_then(Value::as_str) == Some(sha))
        .max_by_key(|r| (s(r, "created_at"), r.get("id").and_then(Value::as_u64).unwrap_or(0)))
        .map(|r| {
            (
                r.get("id").and_then(Value::as_u64).unwrap_or(0),
                s(r, "status"),
                s(r, "conclusion"),
                s(r, "html_url"),
            )
        })
}

/// `name (conclusion)` for every job in a `GET actions/runs/{id}/jobs`
/// payload that cancelled or failed, in payload order.
pub fn bad_jobs(jobs: &Value) -> Vec<String> {
    jobs.get("jobs")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|j| {
                    let c = j.get("conclusion").and_then(Value::as_str)?;
                    BAD_JOB.contains(&c).then(|| {
                        format!("{} ({c})", j.get("name").and_then(Value::as_str).unwrap_or("?"))
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a `GET actions/workflows` payload defines a workflow named `CI`
/// (in any state — a disabled `CI` workflow is still a declared gate, not a
/// no-CI policy). `Err` when the payload is unreadable or truncated, because an
/// incomplete list cannot prove absence.
pub fn has_ci_workflow(workflows: &Value) -> Result<bool, String> {
    let arr = workflows
        .get("workflows")
        .and_then(Value::as_array)
        .ok_or("workflow list has no `workflows` array")?;
    if arr
        .iter()
        .any(|w| w.get("name").and_then(Value::as_str) == Some(WORKFLOW_NAME))
    {
        return Ok(true);
    }
    // A record without a string `name` could be the `CI` workflow, so the list
    // cannot establish its absence.
    if let Some(i) = arr
        .iter()
        .position(|w| w.get("name").and_then(Value::as_str).is_none())
    {
        return Err(format!("workflow record {i} has no string `name`"));
    }
    match workflows.get("total_count").and_then(Value::as_u64) {
        Some(n) if n as usize <= arr.len() => Ok(false),
        Some(n) => Err(format!("workflow list truncated ({} of {n} read)", arr.len())),
        None => Err("workflow list has no `total_count`".to_string()),
    }
}

/// The `path` of every workflow named `CI` in a `GET actions/workflows`
/// payload, `None` for a `CI` record without a string `path` (which cannot be
/// checked, so it always applies). Empty when the list proves there is no `CI`
/// workflow; `Err` exactly when [`has_ci_workflow`] errs.
pub fn ci_workflow_paths(workflows: &Value) -> Result<Vec<Option<String>>, String> {
    if !has_ci_workflow(workflows)? {
        return Ok(Vec::new());
    }
    Ok(workflows
        .get("workflows")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|w| w.get("name").and_then(Value::as_str) == Some(WORKFLOW_NAME))
                .map(|w| w.get("path").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

/// What a contents read of one workflow file at one ref answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileAt {
    Present,
    /// The forge answered `Not Found` for this path at this ref.
    Absent,
    /// The read failed any other way: nothing is known.
    Unknown(String),
}

/// Classify a `gh api repos/{o}/{r}/contents/{path}?ref=…` result. Only gh's
/// `Not Found (HTTP 404)` is an absence; a 404 for any other reason (e.g. "No
/// commit found for the ref") or any other failure is [`FileAt::Unknown`].
#[must_use]
pub fn classify_contents_read(read: &Result<String, String>) -> FileAt {
    match read {
        Ok(_) => FileAt::Present,
        Err(e) if e.contains("Not Found (HTTP 404)") => FileAt::Absent,
        Err(e) => FileAt::Unknown(e.clone()),
    }
}

/// Whether the workflow file `path` applies to a PR (#12039): present at the
/// base ref or at the head. `Ok(false)` only on a verified absence at BOTH;
/// `Err` when either read is unknown and neither showed the file present.
pub fn workflow_applies(
    path: &str,
    base_ref: &str,
    head_sha: &str,
    mut file_at: impl FnMut(&str, &str) -> FileAt,
) -> Result<bool, String> {
    let mut unknown = Vec::new();
    for r in [base_ref, head_sha] {
        if r.is_empty() {
            unknown.push(format!("{path}: no ref to read it at"));
            continue;
        }
        match file_at(path, r) {
            FileAt::Present => return Ok(true),
            FileAt::Absent => {}
            FileAt::Unknown(e) => unknown.push(format!("{path}@{r}: {e}")),
        }
    }
    if unknown.is_empty() {
        Ok(false)
    } else {
        Err(unknown.join("; "))
    }
}

/// Decide from the runs payload, a loader for the repository's workflow list
/// (only called when no run for this head exists), and a loader for the chosen
/// run's jobs payload (only called when the run is definitively non-green).
/// Every listed `CI` workflow is taken to apply to the PR; see [`assess_with`].
pub fn assess(
    pr: &str,
    sha: &str,
    runs: &Value,
    workflows: impl FnOnce() -> Result<Value, String>,
    jobs_for: impl FnOnce(u64) -> Result<Value, String>,
) -> Verdict {
    assess_with(pr, sha, runs, workflows, |_| Ok(true), jobs_for)
}

/// [`assess`], with `applies(path)` deciding whether a listed `CI` workflow
/// (by its `path`) is one this PR would run (#12039). It is consulted only
/// when no run for this head exists; a `CI` record without a `path` always
/// applies. `Err` from it keeps the workflow applicable (UNVERIFIED, hold).
pub fn assess_with(
    pr: &str,
    sha: &str,
    runs: &Value,
    workflows: impl FnOnce() -> Result<Value, String>,
    mut applies: impl FnMut(&str) -> Result<bool, String>,
    jobs_for: impl FnOnce(u64) -> Result<Value, String>,
) -> Verdict {
    let Some((id, status, conclusion, url)) = latest_run(runs, sha) else {
        let listed = workflows().and_then(|w| ci_workflow_paths(&w));
        let mut unchecked = Vec::new();
        let mut absent = Vec::new();
        let decided = listed.map(|paths| {
            let any = !paths.is_empty();
            let applicable = paths.iter().any(|p| match p {
                None => true,
                Some(p) => match applies(p) {
                    Ok(true) => true,
                    Ok(false) => {
                        absent.push(p.clone());
                        false
                    }
                    Err(e) => {
                        unchecked.push(e);
                        true
                    }
                },
            });
            (any, applicable)
        });
        return match decided {
            Ok((_, true)) if !unchecked.is_empty() => Verdict::Unverified(format!(
                "no `{WORKFLOW_NAME}` workflow run found for head {sha}, and whether the listed `{WORKFLOW_NAME}` workflow file exists at the PR's base or head could not be read: {}",
                unchecked.join("; ")
            )),
            Ok((_, true)) => Verdict::Unverified(format!(
                "no `{WORKFLOW_NAME}` workflow run found for head {sha}, but the repository defines a `{WORKFLOW_NAME}` workflow"
            )),
            Ok((true, false)) => Verdict::NoCiWorkflow(format!(
                "the repository lists a `{WORKFLOW_NAME}` workflow ({}) but its file exists at neither the PR's base nor head {sha} (registered from another branch, #12039), so there is no CI run to gate on",
                absent.join(", ")
            )),
            Ok((false, _)) => Verdict::NoCiWorkflow(format!(
                "the repository defines no `{WORKFLOW_NAME}` workflow, so there is no CI run to gate head {sha} on"
            )),
            Err(e) => Verdict::Unreadable(format!(
                "no `{WORKFLOW_NAME}` run found for head {sha} and the workflow list could not be read to tell 'no CI' from 'not run yet': {e}"
            )),
        };
    };
    if status != "completed" {
        return Verdict::Unverified(format!(
            "`{WORKFLOW_NAME}` run {id} for head {sha} is still {status}"
        ));
    }
    if conclusion == "success" {
        return Verdict::Clean;
    }
    let listed = match jobs_for(id) {
        Ok(j) => bad_jobs(&j),
        Err(e) => {
            eprintln!("Warning: could not list jobs of CI run {id}: {e}");
            Vec::new()
        }
    };
    let token = failed_line(id, &listed);
    let jobs = if listed.is_empty() {
        "no individual failed/cancelled job could be listed".to_string()
    } else {
        listed.join(", ")
    };
    Verdict::Refuse(format!(
        "Merge blocked: PR #{pr}'s head {sha} has a `{WORKFLOW_NAME}` run ({url}) that concluded `{conclusion}`, not `success` (#10444). \
Required contexts being green is not enough: a cancelled or never-acquired job (e.g. Detect Changes) silently skips the lint and test jobs behind it. \
Failed/cancelled jobs: {jobs}. \
Re-run in place with `gh run rerun --failed {id}` (never cancel a run), wait for it to conclude `success`, then merge again.\n{token}"
    ))
}

/// `gh api <path>` (REST, read-only) through the counted `gh` facade.
pub fn gh_get(path: &str) -> Result<String, String> {
    let out = crate::gh_invocation::GhInvocation::new(
        crate::gh_invocation::Operation::new("merge_guard.ci_result"),
        crate::gh_invocation::AccessIntent::Read,
        crate::gh_invocation::GhTarget::None,
        std::time::Duration::from_secs(60),
    )
    .program(crate::gh_invocation::gh_bin())
    .arg("api")
    .arg(path)
    .run();
    match out {
        crate::cmd_out::CmdOutcome::Ran(o) if o.status.success() => {
            Ok(String::from_utf8_lossy(&o.stdout).into_owned())
        }
        crate::cmd_out::CmdOutcome::Ran(o) => {
            Err(format!("gh api {path} failed: {}", String::from_utf8_lossy(&o.stderr).trim()))
        }
        crate::cmd_out::CmdOutcome::Unavailable(u) => Err(format!("could not exec gh api: {u}")),
    }
}

#[cfg(test)]
mod tests;
