//! Feed-driven single-run capture (issue #9201).
//!
//! When the forge event feed reports `workflow_run.completed` for repo R and
//! run N ([`crate::forge_events::keys`]), this records **that one run**
//! through the exact path the repo sweep uses ([`super::record_run`]):
//! fetch the run, list its jobs, then ledger commit, emit, story stitching
//! and wanted-log bookkeeping. About two requests per finished run, instead
//! of a sweep over every repo of every owner.
//!
//! # What is trusted
//!
//! Nothing from the feed but the *choice* of run. The run is re-read from
//! GitHub, and completion, attempt, conclusion, repository (and its
//! visibility) come from that answer. A key is dropped without a request when
//! its repo is not under a configured owner or is excluded; a fetched run is
//! dropped when the forge's `repository.full_name` disagrees with the key or
//! the run is not `completed` (the sweep's correction floor picks it up
//! later). Exactly-once is the ledger's, unchanged: a run the sweep already
//! recorded is `Seen` here and costs one request, and vice versa.
//!
//! # What it never does
//!
//! It never moves a repo's watermark (the sweep owns it), and never replaces
//! the sweep — [`crate::ci_telemetry::feed`] keeps the sweep running as the
//! correctness floor whatever this path does.

use serde::Deserialize;

use super::{
    backoff_until, capture_logs, is_credential_rejection, notify_breaker, record_run, recover,
    CycleContext, CycleError, CycleReport, RepoError, RunOutcome,
};
use crate::ci_telemetry::api::{ApiError, GithubApi};
use crate::ci_telemetry::journal::Journal;
use crate::ci_telemetry::ledger::Ledger;
use crate::ci_telemetry::records::{RepoJson, RunJson};
use crate::ci_telemetry::state::{self, CycleLock, CycleSummary};
use crate::ci_telemetry::story::RepoStories;
use crate::ci_telemetry::{journal_path, state_dir};
use crate::forge_events::keys::RunKey;

/// At most this many runs are fetched per batch. A burst beyond it is not
/// lost — the correction-floor sweep lists whatever this skipped — it is only
/// not captured *early*. Bounds what a noisy or hostile feed can spend.
pub const MAX_RUNS_PER_BATCH: usize = 50;

/// What one targeted batch did.
#[derive(Debug, Clone, Default)]
pub struct TargetedReport {
    /// Requests, emitted runs/jobs, log and story counters — the sweep's own
    /// summary type, so the two paths report in one vocabulary.
    pub summary: CycleSummary,
    /// Runs recorded (committed and emitted) by this batch.
    pub recorded: usize,
    /// Keys whose run was already in the ledger.
    pub already_seen: usize,
    /// Keys dropped, each as `"owner/repo#run: reason"` — not owned,
    /// excluded, over the batch cap, a repo mismatch, not completed, a job
    /// still running, or a per-run API failure.
    pub dropped: Vec<String>,
}

impl TargetedReport {
    #[must_use]
    pub fn summary_line(&self) -> String {
        format!(
            "feed batch: recorded {} run(s) + {} job(s), {} already seen, {} dropped, {} request(s)",
            self.recorded,
            self.summary.jobs_emitted,
            self.already_seen,
            self.dropped.len(),
            self.summary.requests
        )
    }
}

/// The single-run response: the run itself plus GitHub's own `repository`
/// object, which is where the recorded repo identity comes from.
#[derive(Debug, Deserialize)]
struct RunWithRepo {
    #[serde(default)]
    repository: Option<RepoJson>,
}

fn run_path(repo: &str, run_id: u64) -> String {
    format!("repos/{repo}/actions/runs/{run_id}")
}

/// `true` when `repo` (`owner/name`) sits under one of the configured owners
/// and is not excluded — the same scope the sweep polls.
fn in_scope(ctx: &CycleContext<'_>, repo: &str) -> Result<(), &'static str> {
    let (owner, name) = repo.split_once('/').ok_or("not owner/name")?;
    if !ctx
        .owners
        .iter()
        .any(|o| o.login.eq_ignore_ascii_case(owner))
    {
        return Err("not under a configured owner");
    }
    let excluded = ctx
        .excluded_repos
        .iter()
        .any(|x| x.eq_ignore_ascii_case(repo) || x.eq_ignore_ascii_case(name));
    if excluded {
        return Err("excluded repo");
    }
    Ok(())
}

/// Record exactly the runs `keys` name, then run the log pass (when log
/// capture is on). Takes the same per-host cycle lock as the sweep, so the
/// two never interleave; `Busy` means the caller should retry the keys later.
pub fn record_runs(
    ctx: &CycleContext<'_>,
    api: &dyn GithubApi,
    keys: &[RunKey],
) -> Result<TargetedReport, CycleError> {
    let dir = state_dir(ctx.root);
    let Some(_lock) = CycleLock::try_acquire(&dir)? else {
        return Err(CycleError::Busy);
    };
    let status = state::load_status(&dir);
    let mut ledger = Ledger::open(dir.join("seen.jsonl"))?;
    let journal = Journal::open(journal_path(ctx.root))?;
    let mut report = TargetedReport::default();
    report.summary.recovered_units = recover(&mut ledger, &journal)?;
    if let Some(until) = status.backoff_until.filter(|until| *until > ctx.now) {
        return Err(CycleError::BackingOff { until: Some(until) });
    }
    if crate::rate_limit_breaker::global_skip_pass("ci_telemetry") {
        return Err(CycleError::BackingOff { until: None });
    }

    let mut cycle = CycleReport::default();
    let mut fetched = 0usize;
    for key in keys {
        let label = format!("{}#{}", key.repo, key.run_id);
        if let Err(reason) = in_scope(ctx, &key.repo) {
            report.dropped.push(format!("{label}: {reason}"));
            continue;
        }
        if fetched >= MAX_RUNS_PER_BATCH {
            report
                .dropped
                .push(format!("{label}: over the {MAX_RUNS_PER_BATCH}-run batch cap"));
            continue;
        }
        fetched += 1;
        match record_one(ctx, api, &mut ledger, &journal, key, &mut cycle) {
            Ok(Recorded::Run(RunOutcome::Recorded)) => report.recorded += 1,
            Ok(Recorded::Run(RunOutcome::Seen)) => report.already_seen += 1,
            Ok(Recorded::Run(RunOutcome::Held)) => {
                report
                    .dropped
                    .push(format!("{label}: a job is still running"));
            }
            Ok(Recorded::Skipped(reason)) => report.dropped.push(format!("{label}: {reason}")),
            Err(RepoError::Io(error)) => return Err(CycleError::Io(error)),
            Err(RepoError::Api(error)) => {
                if let Some(abort) = org_wide(ctx, &dir, &error, &cycle) {
                    return Err(abort);
                }
                report.dropped.push(format!("{label}: {error}"));
            }
        }
    }
    if ctx.log_capture.is_on() {
        if let Err(error) = capture_logs(ctx, api, &mut ledger, &journal, &mut cycle) {
            if let Some(abort) = org_wide(ctx, &dir, &error, &cycle) {
                return Err(abort);
            }
            report.dropped.push(format!("job-log capture: {error}"));
        }
    }
    let recovered = report.summary.recovered_units;
    report.summary = CycleSummary {
        recovered_units: recovered,
        ..cycle.summary
    };
    Ok(report)
}

enum Recorded {
    Run(RunOutcome),
    Skipped(&'static str),
}

fn record_one(
    ctx: &CycleContext<'_>,
    api: &dyn GithubApi,
    ledger: &mut Ledger,
    journal: &Journal,
    key: &RunKey,
    cycle: &mut CycleReport,
) -> Result<Recorded, RepoError> {
    let path = run_path(&key.repo, key.run_id);
    cycle.summary.requests += 1;
    let response = api.get(&path, None)?;
    let parse = |detail: serde_json::Error| ApiError::Parse {
        path: path.clone(),
        detail: detail.to_string(),
    };
    let run: RunJson = serde_json::from_str(&response.body).map_err(parse)?;
    let Some(repo) = serde_json::from_str::<RunWithRepo>(&response.body)
        .map_err(parse)?
        .repository
    else {
        return Ok(Recorded::Skipped("the forge returned no repository for the run"));
    };
    if !repo.full_name.eq_ignore_ascii_case(&key.repo) || run.id != key.run_id {
        return Ok(Recorded::Skipped("the forge's run does not match the feed key"));
    }
    if !run.is_completed() {
        return Ok(Recorded::Skipped("the run is not completed on the forge"));
    }
    let stories = ctx.repo_identity.map(|resolve| {
        let mut graphql_ok = !cycle.graphql_suppressed;
        let stories = RepoStories::prepare(
            resolve,
            api,
            &repo.full_name,
            &[&run],
            &mut cycle.summary.requests,
            &mut graphql_ok,
        );
        cycle.graphql_suppressed = !graphql_ok;
        stories
    });
    record_run(ctx, api, ledger, journal, &repo, &run, stories.as_ref(), cycle).map(Recorded::Run)
}

/// The sweep's org-wide abort rule, applied to a targeted batch: a rate limit
/// backs the **whole** poller off (persisted, so the next sweep honours it
/// too) and notifies the shared breaker; a rejected credential aborts without
/// either (#8850). Anything else is per-run.
fn org_wide(
    ctx: &CycleContext<'_>,
    dir: &std::path::Path,
    error: &ApiError,
    cycle: &CycleReport,
) -> Option<CycleError> {
    if is_credential_rejection(error) {
        return Some(CycleError::CredentialRejected {
            detail: error.to_string(),
            progress: Box::new(cycle.summary.clone()),
        });
    }
    let ApiError::RateLimited {
        retry_after_secs,
        reset_epoch,
        detail,
    } = error
    else {
        return None;
    };
    notify_breaker(detail);
    let mut status = state::load_status(dir);
    let until =
        backoff_until(ctx.now, *retry_after_secs, *reset_epoch, status.consecutive_failures);
    status.backoff_until = Some(until);
    status.last_error = Some(format!("rate-limited (feed batch): {detail}"));
    status.last_error_at = Some(ctx.now);
    status.consecutive_failures += 1;
    if let Err(io) = state::save_status(dir, &status) {
        log::warn!("ci_telemetry: could not persist the feed batch's backoff: {io}");
    }
    Some(CycleError::RateLimited {
        until,
        detail: detail.clone(),
    })
}
