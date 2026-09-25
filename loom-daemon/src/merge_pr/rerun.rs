//! Re-run stale required checks IN PLACE (#8914) — the remedy for the #8248
//! freshness guard that keeps the head SHA, and with it the Judge verdict.
//!
//! # Why this runs before the #8508 push
//!
//! [`super::redate`] produces fresh evidence by pushing a tree-identical no-op
//! commit. That works with only `contents: write`, but it moves the head, and
//! the stale-verdict guard (#5686) then clears `loom:pr`: every lap costs a
//! full CI run AND a Judge pass, and a busy `main` restarts the lap before it
//! finishes (PR #8909 lost its approval that way on 2026-09-25 for a review of
//! a byte-identical tree). Re-running the checks where they are needs no new
//! commit — nothing about the PR changes except the checks' timestamps.
//!
//! It needs **Actions: write** on the merge identity. Without it GitHub answers
//! `403 Resource not accessible by integration`, which this module reports as
//! [`RerunOutcome::Refused`] so the caller falls back to the #8508 push —
//! byte-for-byte today's behaviour on an install that has not granted it.
//!
//! # Why the whole workflow run, not the stale jobs
//!
//! GitHub allows ONE re-run per workflow run at a time. After a
//! `POST /actions/jobs/{id}/rerun`, the run is `in_progress` and every further
//! job re-run in it answers `403 The workflow run containing this job is
//! already running`; jobs not re-run are carried into the new attempt with
//! their ORIGINAL `started_at`, so the guard keeps refusing (verified
//! 2026-09-25 on runs 36145858487 and 36152790007). Every required context on
//! this repo lives in the one `ci.yml` run, so per-job re-runs cannot refresh
//! them. `POST /actions/runs/{id}/rerun` re-runs every job of the run in
//! parallel: the fast required jobs come back fresh in about a minute, at the
//! price of re-running the slow non-required suites too. Moving the required
//! checks into their own small workflow would make that cheap (#8919).
//!
//! # The loop
//!
//! Bounded by the caller's wait budget. Each pass re-reads the PR head (a
//! moved head is [`RerunOutcome::HeadMoved`], the #5579 contract) and the
//! guard's own live inputs, then:
//!
//! - every stale required check fresh and none pending → [`RerunOutcome::Fresh`];
//! - a stale required check that is not a GitHub Actions job cannot be re-run
//!   in place → [`RerunOutcome::NotApplicable`] (push fallback);
//! - otherwise re-run each workflow run holding a stale required job.
//!   `already running` is a WAIT, never a missing permission: the run is still
//!   going (ours or the original), and a push there would throw away the
//!   verdict for nothing;
//! - a required job in a run this call re-ran that concludes red is real
//!   evidence (e.g. a tightened ratchet) → [`RerunOutcome::Failed`].
//!
//! Out of budget → [`RerunOutcome::Pending`]: the head and `loom:pr` are
//! untouched and the next pass resumes. Nothing is pushed on any path here,
//! and the #8248 guard is not weakened: `Fresh` means a required check really
//! started at/after the base tip, judged by the same timestamps the guard
//! uses.

use super::redate::gh_api_with;
use super::stale_checks::fetch::live_inputs_with;
use super::stale_checks::CheckRun;
use chrono::{DateTime, Utc};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// The `gh` binary, honoring `LOOM_GH_BIN` — the same seam the sibling modules
/// provide. Tests inject a stub through [`rerun_in_place_with`] instead.
fn gh_bin() -> String {
    std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string())
}

/// Conclusions that make a completed required check red evidence.
const RED: &[&str] = &[
    "failure",
    "timed_out",
    "cancelled",
    "action_required",
    "startup_failure",
];

/// A required context's latest evidence was red, in the named workflow run.
#[derive(Debug, Clone, PartialEq)]
pub struct Red {
    pub check: String,
    pub conclusion: String,
    pub run_id: Option<u64>,
}

/// The per-pass reading of the guard's inputs.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Evaluation {
    /// Workflow runs holding at least one stale green required job.
    pub stale_runs: BTreeSet<u64>,
    /// Stale green required checks that are not GitHub Actions jobs.
    pub stale_unrerunnable: Vec<String>,
    /// Required checks with a run that has not completed yet.
    pub pending: Vec<String>,
    /// Required checks whose latest completed run is red.
    pub red: Vec<Red>,
    /// A green required check with no `started_at` — the guard's own
    /// fail-closed unknown.
    pub unknown: Option<String>,
}

impl Evaluation {
    /// Nothing stale and nothing still running: the guard would pass.
    #[must_use]
    pub fn is_fresh(&self) -> bool {
        self.stale_runs.is_empty()
            && self.stale_unrerunnable.is_empty()
            && self.pending.is_empty()
            && self.unknown.is_none()
    }
}

/// Read the required contexts' evidence the way the #8248 guard does (a
/// required context with no run is left to the forge), but report EVERY stale
/// one — the guard stops at the first — and treat a context with any
/// not-yet-completed run as pending: a re-run's fresh attempt may still be
/// queued with no `started_at` while the old green run is still listed.
#[must_use]
pub fn evaluate(base_tip: DateTime<Utc>, required: &[String], runs: &[CheckRun]) -> Evaluation {
    let mut required_sorted: Vec<&String> = required.iter().collect();
    required_sorted.sort();
    required_sorted.dedup();
    let mut ev = Evaluation::default();
    for ctx in required_sorted {
        let mine: Vec<&CheckRun> = runs.iter().filter(|r| &r.name == ctx).collect();
        if mine.is_empty() {
            continue;
        }
        if mine.iter().any(|r| r.status != "completed") {
            ev.pending.push(ctx.clone());
            continue;
        }
        let Some(latest) = mine.iter().max_by_key(|r| r.started_at) else {
            continue;
        };
        let conclusion = latest.conclusion.as_deref().unwrap_or("");
        if RED.contains(&conclusion) {
            ev.red.push(Red {
                check: ctx.clone(),
                conclusion: conclusion.to_string(),
                run_id: latest.actions_run_id,
            });
            continue;
        }
        if conclusion != "success" {
            continue; // skipped/neutral: never green evidence, never stale.
        }
        match latest.started_at {
            None => {
                if ev.unknown.is_none() {
                    ev.unknown = Some(format!(
                        "required check '{ctx}' is green but reports no started_at, so its \
freshness cannot be determined"
                    ));
                }
            }
            Some(started) if started < base_tip => match latest.actions_run_id {
                Some(run) => {
                    ev.stale_runs.insert(run);
                }
                None => ev.stale_unrerunnable.push(ctx.clone()),
            },
            Some(_) => {}
        }
    }
    ev
}

/// How the forge answered a whole-run re-run request.
#[derive(Debug, Clone, PartialEq)]
pub enum PostResult {
    /// Accepted: the run is re-running.
    Started,
    /// `403 … already running`: the run is still in progress. Wait.
    AlreadyRunning,
    /// Any other 403/404 — missing Actions: write, a run too old to re-run,
    /// a run that no longer exists. The push fallback can still help.
    Refused(String),
    /// Anything else (network, 5xx, rate limit): leave the refusal standing.
    Error(String),
}

/// Classify a failed `gh api -X POST …/rerun` by its message. The
/// `already running` test comes first: it is a 403 too, and mistaking it for
/// a missing permission is exactly the push this module exists to avoid.
#[must_use]
pub fn classify_post_error(err: &str) -> PostResult {
    let lower = err.to_ascii_lowercase();
    if lower.contains("already running") {
        PostResult::AlreadyRunning
    } else if lower.contains("http 403")
        || lower.contains("http 404")
        || lower.contains("not accessible by integration")
    {
        PostResult::Refused(err.to_string())
    } else {
        PostResult::Error(err.to_string())
    }
}

/// The result of the in-place remedy.
#[derive(Debug, Clone, PartialEq)]
pub enum RerunOutcome {
    /// Every required check is fresh. `reran` lists the workflow runs this
    /// call re-ran (empty if the evidence was already fresh on the first read).
    Fresh { reran: Vec<u64> },
    /// Re-runs are in flight (or waiting on a run still in progress) and the
    /// budget ran out. Head and verdict untouched; the next pass resumes.
    Pending {
        reran: Vec<u64>,
        waiting_on: Vec<String>,
    },
    /// The forge refused the re-run before anything was re-run (most often:
    /// no Actions: write). The caller falls back to the #8508 push.
    Refused(String),
    /// A stale required check is not a GitHub Actions job, so there is no run
    /// to re-run. The caller falls back to the #8508 push.
    NotApplicable(String),
    /// The PR head moved past the SHA the caller gated on (#5579).
    HeadMoved { current: String },
    /// Could not read the inputs, a re-run concluded red, or the forge failed
    /// in a way a push would not fix. The refusal stands.
    Failed(String),
}

/// Run the remedy for `pr` in `nwo`, re-reading every `poll` until the
/// required checks are fresh or `wait` has elapsed. A zero `wait` makes one
/// pass (re-run, then report `Pending` unless already fresh).
pub fn rerun_in_place(
    nwo: &str,
    pr: &str,
    expected_head_sha: &str,
    wait: Duration,
    poll: Duration,
) -> RerunOutcome {
    rerun_in_place_with(&gh_bin(), nwo, pr, expected_head_sha, wait, poll)
}

/// [`rerun_in_place`], parameterized on the `gh` binary.
fn rerun_in_place_with(
    gh: &str,
    nwo: &str,
    pr: &str,
    expected_head_sha: &str,
    wait: Duration,
    poll: Duration,
) -> RerunOutcome {
    let deadline = Instant::now() + wait;
    let mut reran: BTreeSet<u64> = BTreeSet::new();
    loop {
        let (head, base_ref) = match read_pr(gh, nwo, pr) {
            Ok(v) => v,
            Err(e) => return RerunOutcome::Failed(format!("could not read PR #{pr}: {e}")),
        };
        if head != expected_head_sha {
            return RerunOutcome::HeadMoved { current: head };
        }
        let inputs = match live_inputs_with(gh, nwo, &base_ref, &head) {
            Ok(i) => i,
            Err(e) => {
                return RerunOutcome::Failed(format!(
                    "could not read the freshness guard's inputs: {e}"
                ))
            }
        };
        let ev = evaluate(inputs.base_tip, &inputs.required, &inputs.runs);
        if let Some(why) = ev.unknown {
            return RerunOutcome::Failed(why);
        }
        if let Some(red) = ev
            .red
            .iter()
            .find(|r| r.run_id.is_some_and(|id| reran.contains(&id)))
        {
            return RerunOutcome::Failed(format!(
                "required check '{}' concluded {} on its in-place re-run (workflow run {}) — \
that is current evidence against this merge, not a stale timestamp",
                red.check,
                red.conclusion,
                red.run_id.unwrap_or_default()
            ));
        }
        if !ev.stale_unrerunnable.is_empty() {
            let names = ev.stale_unrerunnable.join("', '");
            let why = format!(
                "stale required check(s) '{names}' are not GitHub Actions jobs, so there is no \
workflow run to re-run in place"
            );
            return if reran.is_empty() {
                RerunOutcome::NotApplicable(why)
            } else {
                RerunOutcome::Failed(why)
            };
        }
        if ev.is_fresh() {
            return RerunOutcome::Fresh {
                reran: reran.into_iter().collect(),
            };
        }
        for run_id in &ev.stale_runs {
            match post_rerun(gh, nwo, *run_id) {
                PostResult::Started => {
                    reran.insert(*run_id);
                }
                PostResult::AlreadyRunning => {}
                PostResult::Refused(why) if reran.is_empty() => {
                    return RerunOutcome::Refused(format!(
                        "re-running workflow run {run_id} was refused: {why}"
                    ))
                }
                PostResult::Refused(why) | PostResult::Error(why) => {
                    return RerunOutcome::Failed(format!(
                        "re-running workflow run {run_id} failed: {why}"
                    ))
                }
            }
        }
        if Instant::now() >= deadline {
            let mut waiting_on = ev.pending.clone();
            waiting_on.extend(ev.stale_runs.iter().map(|r| format!("workflow run {r}")));
            return RerunOutcome::Pending {
                reran: reran.into_iter().collect(),
                waiting_on,
            };
        }
        std::thread::sleep(poll);
    }
}

/// The PR's live head SHA and base ref, in one read.
fn read_pr(gh: &str, nwo: &str, pr: &str) -> Result<(String, String), String> {
    let out = gh_api_with(
        gh,
        &[
            &format!("repos/{nwo}/pulls/{pr}"),
            "--jq",
            "[.head.sha, .base.ref] | @tsv",
        ],
    )?;
    let mut cols = out.split('\t');
    let head = cols.next().unwrap_or("").trim().to_string();
    let base = cols.next().unwrap_or("").trim().to_string();
    if head.is_empty() || base.is_empty() {
        return Err("the PR resolved to an empty head SHA or base ref".to_string());
    }
    Ok((head, base))
}

fn post_rerun(gh: &str, nwo: &str, run_id: u64) -> PostResult {
    match gh_api_with(
        gh,
        &[
            "-X",
            "POST",
            &format!("repos/{nwo}/actions/runs/{run_id}/rerun"),
        ],
    ) {
        Ok(_) => PostResult::Started,
        Err(e) => classify_post_error(&e),
    }
}

#[cfg(test)]
mod tests;
