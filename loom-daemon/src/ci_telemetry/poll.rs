//! One poll cycle over the configured org.
//!
//! 1. Take the per-host cycle lock; open (and repair) the ledger + journal.
//! 2. **Recover**: replay any committed-but-unconfirmed unit, skipping every
//!    envelope the journal already holds (the crash-between-commit-and-emit
//!    case — this is what makes emission exactly-once, not at-most-once).
//! 3. Honour the org-wide rate-limit backoff and the process-global rate
//!    limit breaker — a backing-off cycle makes zero requests.
//! 4. Discover repos (`GET /orgs/{org}/repos`, paginated, ETag/304-cached).
//! 5. Per repo: list runs `created >= watermark` (paginated); for each
//!    completed, unseen run attempt list its jobs (`filter=all`, paginated),
//!    **commit** the unseen job units then the run unit to the ledger
//!    (fsync), **emit** them to the journal (fsync), confirm; finally advance
//!    the watermark.
//!
//! A rate limit anywhere aborts the whole cycle (the org backs off, never a
//! single repo). Any other per-repo failure is recorded and the cycle moves on
//! to the next repo; `--once` still exits non-zero naming it.

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};

use super::api::{ApiError, GithubApi};
use super::journal::Journal;
use super::ledger::{Ledger, PendingUnit, UnitDraft, UnitKey, COMPACT_THRESHOLD_BYTES};
use super::records::{
    envelope_identity, job_envelopes, run_envelopes, JobJson, JobsPage, RepoJson, RunJson, RunsPage,
};
use super::state::{self, CycleLock, CycleSummary, PollStatus};
use super::{journal_path, log_capture_gate, state_dir, LogCaptureGate, ResolvedCiTelemetry};

/// Everything a cycle needs, resolved up front (a seam for tests: `now`,
/// `host_id`, and the lookback are injectable).
#[derive(Debug, Clone)]
pub struct CycleContext<'a> {
    pub root: &'a Path,
    pub org: String,
    pub excluded_repos: Vec<String>,
    pub now: DateTime<Utc>,
    pub host_id: String,
    pub initial_lookback: Duration,
    pub log_capture: LogCaptureGate,
}

impl<'a> CycleContext<'a> {
    #[must_use]
    pub fn new(root: &'a Path, resolved: &ResolvedCiTelemetry) -> Self {
        CycleContext {
            root,
            org: resolved.org.clone(),
            excluded_repos: resolved
                .excluded_repos
                .iter()
                .map(|exclusion| exclusion.repo.clone())
                .collect(),
            now: Utc::now(),
            host_id: crate::sweep_registry::host_identity(),
            initial_lookback: Duration::hours(super::INITIAL_LOOKBACK_HOURS),
            log_capture: log_capture_gate(resolved),
        }
    }

    fn is_excluded(&self, repo: &RepoJson) -> bool {
        self.excluded_repos.iter().any(|excluded| {
            excluded.eq_ignore_ascii_case(&repo.name)
                || excluded.eq_ignore_ascii_case(&repo.full_name)
        })
    }
}

/// What a completed cycle did.
#[derive(Debug, Clone, Default)]
pub struct CycleReport {
    pub summary: CycleSummary,
    /// `"owner/repo: reason"` for each repo that failed (non-rate-limit).
    pub repo_errors: Vec<String>,
    /// A torn ledger tail was detected and repaired on open.
    pub ledger_repaired: bool,
}

impl CycleReport {
    #[must_use]
    pub fn summary(&self) -> String {
        let s = &self.summary;
        let mut text = format!(
            "polled {} repo(s): emitted {} run(s) + {} job(s), recovered {} pending unit(s), {} request(s)",
            s.repos_polled, s.runs_emitted, s.jobs_emitted, s.recovered_units, s.requests
        );
        if !self.repo_errors.is_empty() {
            text.push_str(&format!(
                "; {} repo(s) failed: {}",
                self.repo_errors.len(),
                self.repo_errors.join("; ")
            ));
        }
        text
    }
}

/// Why a cycle did not run to completion. Every variant is a named reason.
#[derive(Debug)]
pub enum CycleError {
    /// Another cycle on this host holds the lock.
    Busy,
    /// Still inside an org-wide backoff window (or the global breaker is
    /// cooling) — zero requests were made.
    BackingOff { until: Option<DateTime<Utc>> },
    /// This cycle hit a rate limit; the org backs off until `until`.
    RateLimited {
        until: DateTime<Utc>,
        detail: String,
    },
    /// Repo discovery failed (nothing can be polled without it).
    Discovery(ApiError),
    /// Ledger/journal/state I/O failed.
    Io(io::Error),
}

impl fmt::Display for CycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CycleError::Busy => {
                write!(f, "busy: another ci-telemetry cycle holds the lock on this host")
            }
            CycleError::BackingOff { until: Some(until) } => {
                write!(f, "backing-off: org-wide rate-limit backoff until {}", until.to_rfc3339())
            }
            CycleError::BackingOff { until: None } => {
                write!(f, "backing-off: the daemon's rate-limit breaker is cooling")
            }
            CycleError::RateLimited { until, detail } => write!(
                f,
                "rate-limited: backing off the whole org until {} ({detail})",
                until.to_rfc3339()
            ),
            CycleError::Discovery(error) => write!(f, "discovery-failed: {error}"),
            CycleError::Io(error) => write!(f, "io-failed: {error}"),
        }
    }
}

impl std::error::Error for CycleError {}

impl From<io::Error> for CycleError {
    fn from(error: io::Error) -> Self {
        CycleError::Io(error)
    }
}

/// Internal per-repo failure: an API error is recorded (or aborts on a rate
/// limit); an I/O error is fatal to the cycle.
enum RepoError {
    Api(ApiError),
    Io(io::Error),
}

impl From<ApiError> for RepoError {
    fn from(error: ApiError) -> Self {
        RepoError::Api(error)
    }
}

impl From<io::Error> for RepoError {
    fn from(error: io::Error) -> Self {
        RepoError::Io(error)
    }
}

/// Backoff for a rate limit: `Retry-After` wins, then the primary-limit
/// reset epoch, then exponential (60s doubling, capped at 1h).
#[must_use]
pub fn backoff_until(
    now: DateTime<Utc>,
    retry_after_secs: Option<u64>,
    reset_epoch: Option<i64>,
    consecutive_failures: u32,
) -> DateTime<Utc> {
    if let Some(secs) = retry_after_secs {
        return now + Duration::seconds(i64::try_from(secs).unwrap_or(3600).max(1));
    }
    if let Some(epoch) = reset_epoch.and_then(|e| DateTime::<Utc>::from_timestamp(e, 0)) {
        return epoch.max(now + Duration::seconds(60));
    }
    let exponent = consecutive_failures.min(6);
    now + Duration::seconds((60_i64 << exponent).min(3600))
}

/// Run one cycle and record its outcome in `status.json`.
pub fn run_cycle(ctx: &CycleContext<'_>, api: &dyn GithubApi) -> Result<CycleReport, CycleError> {
    let dir = state_dir(ctx.root);
    let Some(_lock) = CycleLock::try_acquire(&dir)? else {
        return Err(CycleError::Busy);
    };
    if ctx.log_capture == LogCaptureGate::RefusedNotImplemented {
        log::warn!("ci_telemetry: logCaptureEnabled {}", ctx.log_capture.as_str());
    }
    let mut status = state::load_status(&dir);
    let result = run_locked(ctx, api, &dir, &status);
    if matches!(result, Err(CycleError::BackingOff { .. })) {
        // A skipped cycle made no attempt; the failing status that caused
        // the backoff stays exactly as recorded.
        return result;
    }
    record_outcome(&mut status, ctx, &result);
    state::save_status(&dir, &status)?;
    result
}

fn record_outcome(
    status: &mut PollStatus,
    ctx: &CycleContext<'_>,
    result: &Result<CycleReport, CycleError>,
) {
    status.org = Some(ctx.org.clone());
    status.last_attempt_at = Some(ctx.now);
    match result {
        Ok(report) => {
            status.last_cycle = Some(CycleSummary {
                repo_errors: report.repo_errors.len(),
                ..report.summary.clone()
            });
            if report.repo_errors.is_empty() {
                status.last_ok_at = Some(ctx.now);
                status.consecutive_failures = 0;
                status.backoff_until = None;
            } else {
                status.last_error = Some(format!(
                    "{} repo(s) failed; first: {}",
                    report.repo_errors.len(),
                    report.repo_errors[0]
                ));
                status.last_error_at = Some(ctx.now);
                status.consecutive_failures += 1;
            }
        }
        Err(error) => {
            status.last_error = Some(error.to_string());
            status.last_error_at = Some(ctx.now);
            status.consecutive_failures += 1;
            if let CycleError::RateLimited { until, .. } = error {
                status.backoff_until = Some(*until);
            }
        }
    }
}

fn run_locked(
    ctx: &CycleContext<'_>,
    api: &dyn GithubApi,
    dir: &Path,
    status: &PollStatus,
) -> Result<CycleReport, CycleError> {
    let mut ledger = Ledger::open(dir.join("seen.jsonl"))?;
    let journal = Journal::open(journal_path(ctx.root))?;
    let mut report = CycleReport {
        ledger_repaired: ledger.repaired(),
        ..CycleReport::default()
    };
    report.summary.recovered_units = recover(&mut ledger, &journal)?;

    if let Some(until) = status.backoff_until.filter(|until| *until > ctx.now) {
        return Err(CycleError::BackingOff { until: Some(until) });
    }
    if crate::rate_limit_breaker::global_is_suppressed() {
        return Err(CycleError::BackingOff { until: None });
    }

    let rate_limited = |error: &ApiError| -> Option<CycleError> {
        if let ApiError::RateLimited {
            retry_after_secs,
            reset_epoch,
            detail,
        } = error
        {
            crate::rate_limit_breaker::global_observe_failure(detail, "ci_telemetry");
            return Some(CycleError::RateLimited {
                until: backoff_until(
                    ctx.now,
                    *retry_after_secs,
                    *reset_epoch,
                    status.consecutive_failures,
                ),
                detail: detail.clone(),
            });
        }
        None
    };

    let repos = match discover(api, &ctx.org, dir, &mut report.summary.requests) {
        Ok(repos) => repos,
        Err(error) => return Err(rate_limited(&error).unwrap_or(CycleError::Discovery(error))),
    };
    for repo in repos.iter().filter(|r| !r.archived && !ctx.is_excluded(r)) {
        match poll_repo(ctx, api, &mut ledger, &journal, repo, &mut report) {
            Ok(()) => report.summary.repos_polled += 1,
            Err(RepoError::Io(error)) => return Err(CycleError::Io(error)),
            Err(RepoError::Api(error)) => {
                if let Some(abort) = rate_limited(&error) {
                    return Err(abort);
                }
                report
                    .repo_errors
                    .push(format!("{}: {error}", repo.full_name));
            }
        }
    }
    if let Err(error) = ledger.compact_if_large(COMPACT_THRESHOLD_BYTES) {
        log::warn!("ci_telemetry: ledger compaction failed (will retry next cycle): {error}");
    }
    Ok(report)
}

/// Replay committed-but-unconfirmed units: append only the envelopes the
/// journal does not already hold, then confirm. Returns the unit count.
fn recover(ledger: &mut Ledger, journal: &Journal) -> io::Result<usize> {
    let pending: Vec<PendingUnit> = ledger.pending().to_vec();
    if pending.is_empty() {
        return Ok(0);
    }
    let present = journal.identities()?;
    let missing: Vec<_> = pending
        .iter()
        .flat_map(|unit| unit.envelopes.iter())
        .filter(|env| envelope_identity(env).is_none_or(|id| !present.contains(&id)))
        .cloned()
        .collect();
    log::info!(
        "ci_telemetry: recovering {} committed-but-unconfirmed unit(s) ({} envelope(s) missing from the journal)",
        pending.len(),
        missing.len()
    );
    journal.append(&missing)?;
    if let Some(through) = pending.iter().map(|u| u.seq).max() {
        ledger.mark_emitted(through)?;
    }
    Ok(pending.len())
}

/// Follow `rel="next"` from `first`, collecting each page's parsed items.
fn paginate<T>(
    api: &dyn GithubApi,
    first: String,
    requests: &mut usize,
    parse: impl Fn(&str) -> Result<Vec<T>, serde_json::Error>,
) -> Result<Vec<T>, ApiError> {
    let mut items = Vec::new();
    let mut visited = HashSet::new();
    let mut next = Some(first);
    while let Some(path) = next.take() {
        if !visited.insert(path.clone()) {
            break;
        }
        *requests += 1;
        let response = api.get(&path, None)?;
        items.extend(parse(&response.body).map_err(|e| ApiError::Parse {
            path: path.clone(),
            detail: e.to_string(),
        })?);
        next = response.next;
    }
    Ok(items)
}

/// Discover the org's repos, paginated, with a persisted per-page ETag
/// cache: a `304` serves the cached page (and its cached `next`) at zero
/// rate-limit cost — the `forge_listing` ETag mechanism, never a raw
/// re-listing.
pub fn discover(
    api: &dyn GithubApi,
    org: &str,
    dir: &Path,
    requests: &mut usize,
) -> Result<Vec<RepoJson>, ApiError> {
    let mut cache = state::load_discovery_cache(dir);
    let mut refreshed = state::DiscoveryCache::new();
    let mut repos = Vec::new();
    let mut visited = HashSet::new();
    let mut next = Some(format!("orgs/{org}/repos?per_page=100&type=all"));
    while let Some(path) = next.take() {
        if !visited.insert(path.clone()) {
            break;
        }
        let cached = cache.remove(&path);
        *requests += 1;
        let mut response = api.get(&path, cached.as_ref().map(|c| c.etag.as_str()))?;
        let page = match (response.status, cached) {
            (304, Some(page)) => page,
            (304, None) => {
                // Unreachable in practice (a 304 only answers our ETag);
                // re-fetch unconditionally rather than trust an empty body.
                *requests += 1;
                response = api.get(&path, None)?;
                state::CachedPage {
                    etag: response.etag.clone().unwrap_or_default(),
                    body: response.body.clone(),
                    next: response.next.clone(),
                }
            }
            _ => state::CachedPage {
                etag: response.etag.clone().unwrap_or_default(),
                body: response.body.clone(),
                next: response.next.clone(),
            },
        };
        let rows: Vec<RepoJson> =
            serde_json::from_str(&page.body).map_err(|e| ApiError::Parse {
                path: path.clone(),
                detail: e.to_string(),
            })?;
        repos.extend(rows);
        next = page.next.clone();
        if !page.etag.is_empty() {
            refreshed.insert(path, page);
        }
    }
    if let Err(error) = state::save_discovery_cache(dir, &refreshed) {
        log::warn!("ci_telemetry: could not persist the discovery ETag cache: {error}");
    }
    Ok(repos)
}

/// Hold the watermark at the oldest not-yet-finished run so a later poll
/// still lists it once it completes.
fn hold(at: DateTime<Utc>, oldest: &mut Option<DateTime<Utc>>) {
    *oldest = Some(oldest.map_or(at, |o| o.min(at)));
}

fn runs_path(repo: &str, watermark: DateTime<Utc>) -> String {
    // `created=>=<ts>`, percent-encoded so `gh api` passes it verbatim.
    format!(
        "repos/{repo}/actions/runs?per_page=100&created=%3E%3D{}",
        watermark.format("%Y-%m-%dT%H:%M:%SZ")
    )
}

fn jobs_path(repo: &str, run_id: u64) -> String {
    format!("repos/{repo}/actions/runs/{run_id}/jobs?filter=all&per_page=100")
}

fn poll_repo(
    ctx: &CycleContext<'_>,
    api: &dyn GithubApi,
    ledger: &mut Ledger,
    journal: &Journal,
    repo: &RepoJson,
    report: &mut CycleReport,
) -> Result<(), RepoError> {
    let full = repo.full_name.as_str();
    let watermark = ledger
        .watermark(full)
        .unwrap_or(ctx.now - ctx.initial_lookback);
    let mut runs: Vec<RunJson> =
        paginate(api, runs_path(full, watermark), &mut report.summary.requests, |body| {
            serde_json::from_str::<RunsPage>(body).map(|p| p.workflow_runs)
        })?;
    runs.sort_by_key(|r| (r.created_at, r.id));

    let mut newest = watermark;
    let mut oldest_incomplete: Option<DateTime<Utc>> = None;
    for run in &runs {
        newest = newest.max(run.created_at);
        if !run.is_completed() {
            hold(run.created_at, &mut oldest_incomplete);
            continue;
        }
        let run_key = UnitKey::run(full, run.id, run.run_attempt);
        if ledger.is_seen(&run_key) {
            continue;
        }
        let jobs: Vec<JobJson> =
            paginate(api, jobs_path(full, run.id), &mut report.summary.requests, |body| {
                serde_json::from_str::<JobsPage>(body).map(|p| p.jobs)
            })?;
        if jobs.iter().any(|job| !job.is_completed()) {
            hold(run.created_at, &mut oldest_incomplete);
            continue;
        }
        // Job units first, the run unit LAST: a torn commit can then only
        // lose the run unit, leaving the run "unseen" so the next poll
        // re-lists its jobs and commits exactly the missing ones.
        let mut drafts: Vec<UnitDraft> = jobs
            .iter()
            .map(|job| UnitDraft {
                key: UnitKey::job(full, run.id, job.id, job.run_attempt),
                envelopes: job_envelopes(repo, run, job, &ctx.host_id),
            })
            .filter(|draft| !ledger.is_seen(&draft.key))
            .collect();
        drafts.push(UnitDraft {
            key: run_key,
            envelopes: run_envelopes(repo, run, &ctx.host_id),
        });
        let committed = ledger.commit(drafts)?;
        emit(ledger, journal, &committed)?;
        for unit in &committed {
            if unit.key.job_id.is_some() {
                report.summary.jobs_emitted += 1;
            } else {
                report.summary.runs_emitted += 1;
            }
        }
    }
    ledger.set_watermark(full, oldest_incomplete.unwrap_or(newest))?;
    Ok(())
}

/// Emit freshly committed units to the journal, then confirm them.
fn emit(ledger: &mut Ledger, journal: &Journal, committed: &[PendingUnit]) -> io::Result<()> {
    let envelopes: Vec<_> = committed
        .iter()
        .flat_map(|u| u.envelopes.iter().cloned())
        .collect();
    journal.append(&envelopes)?;
    if let Some(through) = committed.iter().map(|u| u.seq).max() {
        ledger.mark_emitted(through)?;
    }
    Ok(())
}
