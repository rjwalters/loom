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
//! | latest `CI` run concluded `success` | [`CLEAN`] | 0 |
//! | no run found / run still in progress | [`UNVERIFIED`] + reason | 0 |
//! | latest run concluded anything else | the refusal (names the cancelled / failed jobs, hints `gh run rerun --failed`) | 1 |
//! | could not query the forge | the reason | 2 |
//!
//! Only a DEFINITE non-success refuses. An unanswered query (exit 2, or an
//! older binary) is a warning at the caller, never an all-clear claim; an
//! in-progress run is the check-wait loops' business, not a verdict.
//!
//! The remedy is `gh run rerun --failed <run>`: it re-runs in place, so no
//! distinct commit's run is ever cancelled (ci-principles.md).

use serde_json::Value;

/// Stdout of a clean verdict.
pub const CLEAN: &str = "LOOM-CI-RESULT-CLEAN";
/// Stdout prefix when no definite verdict exists (callers warn, not refuse).
pub const UNVERIFIED: &str = "LOOM-CI-RESULT-UNVERIFIED";

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
    Unverified(String),
    Refuse(String),
}

/// The newest `CI` run in a `GET actions/runs?head_sha=` payload:
/// `(id, status, conclusion, html_url)`.
pub fn latest_run(runs: &Value) -> Option<(u64, String, String, String)> {
    let arr = runs.get("workflow_runs")?.as_array()?;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    arr.iter()
        .filter(|r| r.get("name").and_then(Value::as_str) == Some(WORKFLOW_NAME))
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

/// Decide from the runs payload and a loader for the chosen run's jobs payload
/// (only called when the run is definitively non-green).
pub fn assess(
    pr: &str,
    sha: &str,
    runs: &Value,
    jobs_for: impl FnOnce(u64) -> Result<Value, String>,
) -> Verdict {
    let Some((id, status, conclusion, url)) = latest_run(runs) else {
        return Verdict::Unverified(format!(
            "no `{WORKFLOW_NAME}` workflow run found for head {sha}"
        ));
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
    let jobs = if listed.is_empty() {
        "no individual failed/cancelled job could be listed".to_string()
    } else {
        listed.join(", ")
    };
    Verdict::Refuse(format!(
        "Merge blocked: PR #{pr}'s head {sha} has a `{WORKFLOW_NAME}` run ({url}) that concluded `{conclusion}`, not `success` (#10444). \
Required contexts being green is not enough: a cancelled or never-acquired job (e.g. Detect Changes) silently skips the lint and test jobs behind it. \
Failed/cancelled jobs: {jobs}. \
Re-run in place with `gh run rerun --failed {id}` (never cancel a run), wait for it to conclude `success`, then merge again."
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
