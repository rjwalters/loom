//! One poll cycle over the configured org.
//!
//! 1. Take the per-host cycle lock; open (and repair) the ledger + journal.
//! 2. **Recover**: replay any committed-but-unconfirmed unit, skipping every
//!    envelope the journal already holds (the crash-between-commit-and-emit
//!    case — this is what makes emission exactly-once, not at-most-once).
//! 3. Honour the org-wide rate-limit backoff and the process-global rate
//!    limit breaker — a backing-off cycle makes zero requests.
//! 4. Discover repos (`GET /orgs/{org}/repos`, paginated, ETag/304-cached).
//! 5. Per repo: list runs `created >= floor` (paginated), where the floor is
//!    the watermark *or* the trailing rescan window, whichever is older
//!    (#8898 — a re-run keeps its original `created_at`, so the watermark
//!    alone would hide it); for each completed, unseen run attempt list its
//!    jobs (`filter=all`, paginated), **commit** the unseen job units then
//!    the run unit to the ledger (fsync), **emit** them to the journal
//!    (fsync), confirm; finally advance the watermark.
//!
//! A rate limit anywhere aborts the whole cycle (the org backs off, never a
//! single repo). So does a rejected credential (#8850) — a property of the
//! host, not the repo, so every remaining repo would fail identically — but
//! under its own named reason and without touching the rate-limit breaker.
//! Any other per-repo failure is recorded and the cycle moves on to the next
//! repo; `--once` still exits non-zero naming it.

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};

use super::api::{ApiError, GithubApi};
use super::journal::Journal;
use super::ledger::{Ledger, PendingUnit, UnitDraft, UnitKey, COMPACT_THRESHOLD_BYTES};
use super::logs::{self, LogTarget};
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
    /// How far back every poll re-lists runs regardless of the watermark, so
    /// a re-attempt of an older run is still seen (#8898).
    pub rescan_window: Duration,
    pub log_capture: LogCaptureGate,
    /// Repos whose logs are not captured (their records/metrics still are).
    pub log_excluded_repos: Vec<String>,
    /// Per-job cap on captured log text (#8825).
    pub log_max_bytes: usize,
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
            rescan_window: Duration::hours(super::RESCAN_WINDOW_HOURS),
            log_capture: log_capture_gate(resolved),
            log_excluded_repos: resolved
                .log_capture_excluded_repos
                .iter()
                .map(|exclusion| exclusion.repo.clone())
                .collect(),
            log_max_bytes: resolved.log_capture_max_bytes,
        }
    }

    fn matches(names: &[String], repo: &RepoJson) -> bool {
        names.iter().any(|name| {
            name.eq_ignore_ascii_case(&repo.name) || name.eq_ignore_ascii_case(&repo.full_name)
        })
    }

    fn is_excluded(&self, repo: &RepoJson) -> bool {
        Self::matches(&self.excluded_repos, repo)
    }

    /// Whether this repo's completed-job logs are captured this cycle.
    fn captures_logs(&self, repo: &RepoJson) -> bool {
        self.log_capture.is_on() && !Self::matches(&self.log_excluded_repos, repo)
    }

    /// Whether an already-wanted job log may still be downloaded, by
    /// `owner/name`. Checked again at download time, not only when the want
    /// was recorded: a log exclusion added today must take effect today, not
    /// after the backlog it was added because of has already been captured.
    fn may_download_log(&self, full_name: &str) -> bool {
        let bare = full_name.rsplit('/').next().unwrap_or(full_name);
        !self
            .log_excluded_repos
            .iter()
            .any(|name| name.eq_ignore_ascii_case(full_name) || name.eq_ignore_ascii_case(bare))
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
    /// At least one job log was downloaded successfully this cycle (#8825).
    pub logs_fetched: bool,
}

impl CycleReport {
    #[must_use]
    pub fn summary(&self) -> String {
        let s = &self.summary;
        let mut text = format!(
            "polled {} repo(s): emitted {} run(s) + {} job(s), recovered {} pending unit(s), {} request(s)",
            s.repos_polled, s.runs_emitted, s.jobs_emitted, s.recovered_units, s.requests
        );
        if s.logs_captured > 0 || s.log_failures > 0 || s.logs_deferred > 0 || s.logs_truncated > 0
        {
            text.push_str(&format!(
                "; logs: {} job(s) captured in {} chunk(s), {} truncated, {} failed, {} deferred",
                s.logs_captured,
                s.job_logs_emitted,
                s.logs_truncated,
                s.log_failures,
                s.logs_deferred
            ));
        }
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
    /// The forge rejected this host's credential (#8850). Aborts the cycle
    /// with no further per-repo request; the operator response is to renew
    /// or rotate the credential, not to wait, so no backoff is recorded and
    /// the rate-limit breaker is not notified. `progress` is what the cycle
    /// had already committed (and emitted) before the credential died.
    CredentialRejected {
        detail: String,
        progress: Box<CycleSummary>,
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
            CycleError::CredentialRejected { detail, progress } => write!(
                f,
                "credential-rejected: the forge rejected this host's credential; aborted the \
org cycle after {} repo(s), {} run(s) + {} job(s) — renew or rotate it ({detail})",
                progress.repos_polled, progress.runs_emitted, progress.jobs_emitted
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

/// Whether an API failure's text says the credential itself was rejected.
///
/// Deliberately narrow — the two messages `gh`/GitHub emit for a dead or
/// insufficient token. A 404 is NOT included: that genuinely is per-repo (a
/// repo the token cannot see, in an org listing it can).
#[must_use]
pub fn indicates_credential_failure(detail: &str) -> bool {
    let lowered = detail.to_ascii_lowercase();
    lowered.contains("bad credentials") || lowered.contains("requires authentication")
}

/// Whether `error` is a credential rejection. Only an HTTP error body or
/// `gh`'s own transport stderr is inspected — never a parse failure, whose
/// detail can quote arbitrary 2xx response content.
fn is_credential_rejection(error: &ApiError) -> bool {
    match error {
        ApiError::Http { detail, .. } | ApiError::Transport(detail) => {
            indicates_credential_failure(detail)
        }
        ApiError::RateLimited { .. } | ApiError::Parse { .. } => false,
    }
}

#[cfg(test)]
thread_local! {
    /// How many times this thread's cycles notified the rate-limit breaker —
    /// the seam the #8850 tests use to prove a credential rejection never does.
    pub(crate) static BREAKER_NOTIFICATIONS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// Tell the process-global rate-limit breaker about a genuine rate limit.
fn notify_breaker(detail: &str) {
    #[cfg(test)]
    BREAKER_NOTIFICATIONS.with(|n| n.set(n.get() + 1));
    crate::rate_limit_breaker::global_observe_failure(detail, "ci_telemetry");
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
            if report.logs_fetched {
                status.last_log_fetch_at = Some(ctx.now);
            }
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
            match error {
                CycleError::RateLimited { until, .. } => status.backoff_until = Some(*until),
                // What was committed before the credential died still counts.
                CycleError::CredentialRejected { progress, .. } => {
                    status.last_cycle = Some((**progress).clone());
                }
                _ => {}
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

    // Whether `error` ends the whole cycle: a rate limit (the org backs off,
    // and the shared breaker hears about it) or a rejected credential (named
    // separately, and deliberately NOT fed to the breaker — a dead token is
    // not a spent quota, #8850).
    let org_wide = |error: &ApiError, report: &CycleReport| -> Option<CycleError> {
        if is_credential_rejection(error) {
            return Some(CycleError::CredentialRejected {
                detail: error.to_string(),
                progress: Box::new(CycleSummary {
                    repo_errors: report.repo_errors.len(),
                    ..report.summary.clone()
                }),
            });
        }
        if let ApiError::RateLimited {
            retry_after_secs,
            reset_epoch,
            detail,
        } = error
        {
            notify_breaker(detail);
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
        Err(error) => return Err(org_wide(&error, &report).unwrap_or(CycleError::Discovery(error))),
    };
    for repo in repos.iter().filter(|r| !r.archived && !ctx.is_excluded(r)) {
        match poll_repo(ctx, api, &mut ledger, &journal, repo, &mut report) {
            Ok(()) => report.summary.repos_polled += 1,
            Err(RepoError::Io(error)) => return Err(CycleError::Io(error)),
            Err(RepoError::Api(error)) => {
                if let Some(abort) = org_wide(&error, &report) {
                    return Err(abort);
                }
                report
                    .repo_errors
                    .push(format!("{}: {error}", repo.full_name));
            }
        }
    }
    // #8825: the log pass runs over the ledger's wanted set, not over this
    // cycle's listings, so a job whose download failed last cycle is retried
    // here even when its repo produced no new runs.
    if ctx.log_capture.is_on() {
        if let Err(error) = capture_logs(ctx, api, &mut ledger, &journal, &mut report) {
            if let Some(abort) = org_wide(&error, &report) {
                return Err(abort);
            }
            report.repo_errors.push(format!("job-log capture: {error}"));
        }
    }
    if let Err(error) = ledger.compact_if_large(COMPACT_THRESHOLD_BYTES) {
        log::warn!("ci_telemetry: ledger compaction failed (will retry next cycle): {error}");
    }
    Ok(report)
}

/// Download and emit the pending job logs, at most
/// [`logs::MAX_DOWNLOADS_PER_CYCLE`] of them.
///
/// Each job is independently idempotent from its `ci.job` record: the chunks
/// commit under their own `logs: true` ledger key, so a failure here leaves
/// the record alone and retries the download next cycle. Only a rate limit or
/// a rejected credential escapes — that aborts the whole cycle, as everywhere
/// else (a dead token is not the job's fault, so it is not recorded as one).
fn capture_logs(
    ctx: &CycleContext<'_>,
    api: &dyn GithubApi,
    ledger: &mut Ledger,
    journal: &Journal,
    report: &mut CycleReport,
) -> Result<(), ApiError> {
    // A repo excluded from log capture *after* its jobs were already wanted
    // must stop being downloaded now, not once the backlog drains. The
    // `log_wanted` lines stay in the ledger (harmless, and the exclusion may
    // be lifted), they simply are not acted on while the exclusion stands.
    let pending: Vec<LogTarget> = ledger
        .pending_logs()
        .into_iter()
        .filter(|target| ctx.may_download_log(&target.repo))
        .collect();
    let total_pending = pending.len();
    for target in pending.into_iter().take(logs::MAX_DOWNLOADS_PER_CYCLE) {
        report.summary.requests += 1;
        let response = match api.get_document(&logs::logs_path(&target.repo, target.job_id)) {
            Ok(response) => response,
            Err(error @ ApiError::RateLimited { .. }) => return Err(error),
            Err(error) if is_credential_rejection(&error) => return Err(error),
            Err(error) => {
                let reason = error.to_string();
                log::warn!(
                    "ci_telemetry: job-log download failed for {} job {}: {reason}",
                    target.repo,
                    target.job_id
                );
                ledger
                    .record_log_failure(&target.repo, target.job_id, &reason)
                    .map_err(|e| ApiError::Transport(e.to_string()))?;
                report.summary.log_failures += 1;
                continue;
            }
        };
        let chunked = logs::chunk(&response.body, ctx.log_max_bytes, logs::CHUNK_BYTES);
        if chunked.truncated {
            report.summary.logs_truncated += 1;
        }
        let envelopes = logs::log_envelopes(&target, &chunked, &ctx.host_id);
        let chunks = envelopes.len();
        let draft = UnitDraft {
            key: UnitKey::job_logs(&target.repo, target.run_id, target.job_id, target.attempt),
            envelopes,
        };
        let committed = ledger
            .commit(vec![draft])
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        emit(ledger, journal, &committed).map_err(|e| ApiError::Transport(e.to_string()))?;
        if !committed.is_empty() {
            report.summary.job_logs_emitted += chunks;
            report.summary.logs_captured += 1;
        }
        report.logs_fetched = true;
    }
    report.summary.logs_deferred =
        total_pending.saturating_sub(logs::MAX_DOWNLOADS_PER_CYCLE.min(total_pending));
    Ok(())
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
///
/// A hold **older than the current watermark** (only reachable through the
/// trailing rescan window, e.g. an in-progress re-attempt of an old run) is
/// clamped away by [`poll_repo`] rather than moving the watermark backwards:
/// the rescan window re-lists that run next cycle anyway, and a watermark that
/// can regress would re-walk arbitrarily much history.
fn hold(at: DateTime<Utc>, oldest: &mut Option<DateTime<Utc>>) {
    *oldest = Some(oldest.map_or(at, |o| o.min(at)));
}

/// The `created >=` floor for a repo's runs listing.
///
/// Without a watermark (a repo's first poll) it is the initial lookback. With
/// one it is the **older** of the watermark and the trailing rescan window
/// (#8898): a re-run keeps the original run's `created_at`, so a pure
/// `created >= watermark` floor stops listing a run as soon as newer runs have
/// advanced the watermark past it — and a later re-attempt of it is then never
/// exported at all. Re-listing the trailing window every cycle costs one more
/// runs page or two per repo; nothing is exported twice because every
/// re-listed attempt is already `is_seen` in the ledger, and an already-seen
/// run is never re-listed for its jobs.
///
/// The window is capped by `initial_lookback` so the floor can never reach
/// further back than the repo's very first cycle already looked.
#[must_use]
pub fn runs_floor(
    watermark: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    initial_lookback: Duration,
    rescan_window: Duration,
) -> DateTime<Utc> {
    let Some(watermark) = watermark else {
        return now - initial_lookback;
    };
    watermark.min(now - rescan_window.min(initial_lookback))
}

fn runs_path(repo: &str, floor: DateTime<Utc>) -> String {
    // `created=>=<ts>`, percent-encoded so `gh api` passes it verbatim.
    format!(
        "repos/{repo}/actions/runs?per_page=100&created=%3E%3D{}",
        floor.format("%Y-%m-%dT%H:%M:%SZ")
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
    let recorded = ledger.watermark(full);
    let watermark = recorded.unwrap_or(ctx.now - ctx.initial_lookback);
    let floor = runs_floor(recorded, ctx.now, ctx.initial_lookback, ctx.rescan_window);
    let mut runs: Vec<RunJson> =
        paginate(api, runs_path(full, floor), &mut report.summary.requests, |body| {
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
        // #8825: record which jobs' logs are wanted, durably, right after the
        // records land. The download itself happens in this cycle's log pass
        // (or a later cycle's), driven off the ledger rather than off this
        // listing — once the run unit is committed the run is "seen" and its
        // jobs are never listed again, so a retry has nowhere else to come
        // from.
        if ctx.captures_logs(repo) {
            let wanted: Vec<LogTarget> = jobs
                .iter()
                .filter(|job| {
                    committed
                        .iter()
                        .any(|unit| unit.key.job_id == Some(job.id) && !unit.key.logs)
                })
                .map(|job| LogTarget {
                    repo: full.to_string(),
                    visibility: repo.visibility(),
                    run_id: run.id,
                    job_id: job.id,
                    attempt: job.run_attempt,
                    workflow: run.workflow(),
                    job: job.name.clone(),
                    completed_at: job
                        .completed_at
                        .unwrap_or(job.started_at.unwrap_or(run.created_at)),
                })
                .collect();
            ledger.want_logs(&wanted)?;
        }
    }
    // `.max(watermark)` keeps the watermark monotonic: the rescan window can
    // surface an unfinished run created *before* it (see [`hold`]), and a
    // regressing watermark would re-walk history every cycle.
    ledger.set_watermark(full, oldest_incomplete.unwrap_or(newest).max(watermark))?;
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
