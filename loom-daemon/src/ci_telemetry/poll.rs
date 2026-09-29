//! One poll cycle over the configured owners (orgs and users, #9188).
//!
//! 1. Take the per-host cycle lock; open (and repair) the ledger + journal.
//! 2. **Recover**: replay any committed-but-unconfirmed unit, skipping every
//!    envelope the journal already holds (the crash-between-commit-and-emit
//!    case — this is what makes emission exactly-once, not at-most-once).
//! 3. Honour the org-wide rate-limit backoff and the process-global rate
//!    limit breaker — a backing-off cycle makes zero requests.
//! 4. Per owner: resolve its kind (declared, cached, or `GET /users/{owner}`)
//!    and discover its repos (`orgs/{o}/repos` or `users/{u}/repos`,
//!    paginated, ETag/304-cached) — see [`super::owners`]. An owner whose
//!    kind or discovery fails is skipped this cycle and named; only when every
//!    owner fails does the cycle fail as `discovery-failed`.
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

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use super::api::{ApiError, GithubApi};
use super::journal::Journal;
use super::ledger::{Ledger, PendingUnit, UnitDraft, UnitKey, COMPACT_THRESHOLD_BYTES};
use super::logs::{self, LogTarget};
use super::owners::{discover, resolve_kind, KindCache, Owner, OwnerStatus};
use super::records::{
    envelope_identity, job_envelopes, parse_shard, run_envelopes, JobCreationBaseline, JobJson,
    JobsPage, RepoJson, RunJson, RunsPage, ShardInfo, ShardKind,
};
use super::state::{self, CycleLock, CycleSummary, PollStatus};
use super::story::{self, RepoIdentityFn, RepoStories, Stitch};
use super::suites::{self, ArtifactsPage};
use super::{journal_path, log_capture_gate, state_dir, LogCaptureGate, ResolvedCiTelemetry};
use crate::telemetry::TelemetryEnvelope;

/// Everything a cycle needs, resolved up front (a seam for tests: `now`,
/// `host_id`, and the lookback are injectable).
#[derive(Debug, Clone)]
pub struct CycleContext<'a> {
    pub root: &'a Path,
    /// The owners polled, in order (#9188).
    pub owners: Vec<Owner>,
    /// Resolved owner kinds; the process-wide cache in production.
    pub owner_kinds: Arc<KindCache>,
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
    /// The D32 `repo_id` resolver for story stitching (#9088); `None` turns
    /// stitching off (no identity probe, no GraphQL request).
    pub repo_identity: Option<RepoIdentityFn>,
}

impl<'a> CycleContext<'a> {
    #[must_use]
    pub fn new(root: &'a Path, resolved: &ResolvedCiTelemetry) -> Self {
        CycleContext {
            root,
            owners: resolved.owners.clone(),
            owner_kinds: super::owners::global_kind_cache(),
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
            repo_identity: Some(story::resolve_repo_identity),
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
    /// Cleared when GraphQL rate-limits a closing-reference lookup (#9088):
    /// no further lookup is made this cycle.
    pub graphql_suppressed: bool,
    /// Each owner's kind / eligible repo count / skip reason (#9188).
    pub owners: Vec<OwnerStatus>,
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
        if s.suite_spans_emitted > 0 || s.suite_artifact_failures > 0 {
            text.push_str(&format!(
                "; suites: {} span(s) from {} shard record(s), {} artifact(s) failed",
                s.suite_spans_emitted, s.suite_records_read, s.suite_artifact_failures
            ));
        }
        let stitched = s.story_runs_stitched
            + s.story_runs_no_candidate
            + s.story_runs_ambiguous
            + s.story_runs_unresolved;
        if stitched > 0 {
            text.push_str(&format!(
                "; story: {} run(s) stitched, {} without candidate, {} ambiguous, {} unresolved",
                s.story_runs_stitched,
                s.story_runs_no_candidate,
                s.story_runs_ambiguous,
                s.story_runs_unresolved
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
    // `org` keeps its pre-#9188 meaning for one owner; several are joined.
    let logins: Vec<&str> = ctx.owners.iter().map(|o| o.login.as_str()).collect();
    status.org = Some(logins.join(","));
    status.last_attempt_at = Some(ctx.now);
    match result {
        Ok(report) => {
            status.owners = report.owners.clone();
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

    // #9188: owners one after another. A rate limit or a rejected credential
    // still aborts the whole cycle; any other kind-probe or discovery failure
    // skips that owner (named) and moves on. Only when no owner could be
    // discovered at all is it the cycle's `discovery-failed` — with a single
    // owner, exactly the pre-#9188 behaviour.
    let mut repos = Vec::new();
    let mut first_failure = None;
    for owner in &ctx.owners {
        let requests = &mut report.summary.requests;
        // Resolve the kind first so a discovery failure can still report it
        // (issue #9197 item 3) — only a failed *kind* probe leaves it `None`.
        let kind_result = resolve_kind(api, owner, &ctx.owner_kinds, requests);
        let (resolved_kind, discovered) = match kind_result {
            Ok(kind) => (Some(kind), discover(api, &owner.login, kind, dir, requests)),
            Err(error) => (None, Err(error)),
        };
        match discovered {
            Ok(found) => {
                let kind = resolved_kind.expect("kind is resolved whenever discovery ran");
                let eligible: Vec<_> = found
                    .into_iter()
                    .filter(|r| !r.archived && !ctx.is_excluded(r))
                    .collect();
                report.owners.push(OwnerStatus {
                    owner: owner.login.clone(),
                    kind: Some(kind),
                    repos: Some(eligible.len()),
                    error: None,
                });
                repos.extend(eligible);
            }
            Err(error) => {
                if let Some(abort) = org_wide(&error, &report) {
                    return Err(abort);
                }
                let reason = format!("discovery-failed: {error}");
                report.owners.push(OwnerStatus {
                    owner: owner.login.clone(),
                    kind: resolved_kind,
                    error: Some(reason.clone()),
                    ..OwnerStatus::default()
                });
                report
                    .repo_errors
                    .push(format!("owner {}: {reason}", owner.login));
                first_failure.get_or_insert(error);
            }
        }
    }
    if let Some(error) = first_failure.filter(|_| report.owners.iter().all(|o| o.error.is_some())) {
        return Err(CycleError::Discovery(error));
    }
    let mut polled = HashSet::new();
    repos.retain(|repo| polled.insert(repo.full_name.to_ascii_lowercase()));
    for repo in &repos {
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

/// Where one artifact is unpacked. Under the poller's own state dir (not
/// `/tmp`) so a host with a full or noexec `/tmp` fails the same way as the
/// ledger would, and so the path is removed by the same cleanup that removes
/// the state dir.
fn artifact_dir(root: &Path, run_id: u64, artifact_id: u64) -> PathBuf {
    state_dir(root)
        .join("artifacts")
        .join(format!("{run_id}-{artifact_id}"))
}

/// The `loom.ci.suite` spans for this run's sharded shell-suite legs, keyed by
/// job id (#9089).
///
/// **Cost gate.** Returns immediately — zero requests — unless this run has at
/// least one *not-yet-emitted* `shell-suite-shard` job. A repo that does not
/// shard shell suites therefore pays nothing for this feature, not even the
/// artifacts listing, and a re-listed run whose jobs are all already seen does
/// not re-download anything.
///
/// **Failure policy.** This runs BEFORE the run's units are committed, so the
/// spans ride in the job units themselves and stay exactly-once with
/// everything else — no second key space and no separate retry pass. The
/// tradeoff is deliberate and bounded in the other direction: a failure here
/// degrades to *no suite spans for this run*, counted in
/// `suite_artifact_failures` and logged, and is never retried. Holding a run's
/// `ci.run`/`ci.job` records hostage to a side artifact would be the worse
/// failure — the records are the primary signal, the suite spans are a
/// refinement of one job in it.
///
/// Only a rate limit or a rejected credential escapes as `Err`, because those
/// are properties of the host and abort the whole cycle wherever they happen.
fn suite_spans_for_run(
    ctx: &CycleContext<'_>,
    api: &dyn GithubApi,
    ledger: &Ledger,
    repo: &RepoJson,
    run: &RunJson,
    jobs: &[JobJson],
    report: &mut CycleReport,
) -> Result<BTreeMap<u64, Vec<TelemetryEnvelope>>, ApiError> {
    let shards: Vec<(u64, ShardInfo)> = jobs
        .iter()
        .map(|job| (job.id, parse_shard(&job.name)))
        .collect();
    let has_unemitted_shard_leg = jobs.iter().zip(&shards).any(|(job, (_, shard))| {
        shard.kind == ShardKind::ShellSuiteShard
            && !ledger.is_seen(&UnitKey::job(&repo.full_name, run.id, job.id, job.run_attempt))
    });
    if !has_unemitted_shard_leg {
        return Ok(BTreeMap::new());
    }

    let artifacts = paginate(
        api,
        suites::artifacts_path(&repo.full_name, run.id),
        &mut report.summary.requests,
        |body| serde_json::from_str::<ArtifactsPage>(body).map(|p| p.artifacts),
    )?;
    let mut by_job: BTreeMap<u64, Vec<TelemetryEnvelope>> = BTreeMap::new();
    for artifact in artifacts
        .iter()
        .filter(|a| suites::is_timings_artifact(a))
        .take(suites::MAX_ARTIFACTS_PER_RUN)
    {
        let dest = artifact_dir(ctx.root, run.id, artifact.id);
        let _ = std::fs::remove_dir_all(&dest);
        if let Err(error) = std::fs::create_dir_all(&dest) {
            log::warn!(
                "ci_telemetry: could not stage suite-timings artifact {} of {} run {}: {error}",
                artifact.name,
                repo.full_name,
                run.id
            );
            report.summary.suite_artifact_failures += 1;
            continue;
        }
        report.summary.requests += 1;
        let downloaded = api.download_artifact(&repo.full_name, run.id, &artifact.name, &dest);
        let text = match downloaded {
            Ok(()) => suites::read_artifact_text(&dest),
            Err(error @ ApiError::RateLimited { .. }) => {
                let _ = std::fs::remove_dir_all(&dest);
                return Err(error);
            }
            Err(error) if is_credential_rejection(&error) => {
                let _ = std::fs::remove_dir_all(&dest);
                return Err(error);
            }
            Err(error) => {
                log::warn!(
                    "ci_telemetry: suite-timings artifact {} of {} run {} could not be downloaded: {error}",
                    artifact.name,
                    repo.full_name,
                    run.id
                );
                None
            }
        };
        let _ = std::fs::remove_dir_all(&dest);
        let Some(text) = text else {
            report.summary.suite_artifact_failures += 1;
            continue;
        };
        match suite_envelopes_from_text(repo, run, jobs, &shards, &text, &ctx.host_id) {
            Ok((job_id, envelopes)) => {
                report.summary.suite_records_read += 1;
                report.summary.suite_spans_emitted += envelopes.len();
                by_job.entry(job_id).or_default().extend(envelopes);
            }
            Err(reason) => {
                log::info!(
                    "ci_telemetry: suite-timings artifact {} of {} run {} produced no spans: {reason}",
                    artifact.name,
                    repo.full_name,
                    run.id
                );
                report.summary.suite_artifact_failures += 1;
            }
        }
    }
    let _ = std::fs::remove_dir(state_dir(ctx.root).join("artifacts"));
    Ok(by_job)
}

/// Parse one artifact's text and build its job's suite spans. Split out so the
/// whole parse → validate → match → emit path is testable without a download.
fn suite_envelopes_from_text(
    repo: &RepoJson,
    run: &RunJson,
    jobs: &[JobJson],
    shards: &[(u64, ShardInfo)],
    text: &str,
    host_id: &str,
) -> Result<(u64, Vec<TelemetryEnvelope>), suites::RejectReason> {
    let timings = suites::parse(text, run.id)?;
    let job_id = suites::match_job(&timings, shards)?;
    let job = jobs
        .iter()
        .find(|job| job.id == job_id)
        .expect("match_job only returns a job id taken from this run's own jobs");
    let shard = parse_shard(&job.name);
    // The same window `job_envelopes` gives the job span, so a suite span is
    // always inside its parent even when the runner's clock disagrees.
    let job_started = job.started_at.unwrap_or(run.created_at);
    let job_ended = job.completed_at.unwrap_or(job_started).max(job_started);
    let workflow = run.workflow();
    let target = suites::SuiteSpanTarget {
        repo: &repo.full_name,
        visibility: repo.visibility(),
        run_id: run.id,
        attempt: job.run_attempt,
        job_id: job.id,
        job: &job.name,
        workflow: &workflow,
        shard,
        job_context: super::records::job_context(&repo.full_name, run.id, job.run_attempt, job.id),
        job_started,
        job_ended,
    };
    Ok((job_id, suites::suite_envelopes(&target, &timings, host_id)))
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

/// Hold the watermark at the oldest not-yet-finished run so a later poll
/// still lists it once it completes — **unless** it is already older than the
/// bound that would ever surface it again (#8992).
///
/// A hold **older than the current watermark** (only reachable through the
/// trailing rescan window, e.g. an in-progress re-attempt of an old run) is
/// clamped away by [`poll_repo`] rather than moving the watermark backwards:
/// the rescan window re-lists that run next cycle anyway, and a watermark that
/// can regress would re-walk arbitrarily much history.
///
/// A run that never reaches a completed state (cancelled-but-stuck, an
/// abandoned workflow, a `queued` run whose runner never arrives) is held
/// every cycle. Without a bound `oldest_incomplete` would stay pinned at that
/// run's `created_at` forever, so [`runs_floor`] would too — every cycle
/// re-listing a window that grows without bound. The bound is the same
/// effective window [`runs_floor`] uses (the rescan window, capped by the
/// initial lookback): once a run is older than that, it can never be
/// re-surfaced by the rescan window either, so continuing to hold it buys
/// nothing but a pinned watermark. Past the bound the run is treated as
/// abandoned — it stops being held, the watermark is free to advance past it,
/// and (once the watermark has advanced) it drops out of future listings
/// too.
fn hold(
    at: DateTime<Utc>,
    now: DateTime<Utc>,
    rescan_window: Duration,
    initial_lookback: Duration,
    oldest: &mut Option<DateTime<Utc>>,
) {
    if now - at > rescan_window.min(initial_lookback) {
        return;
    }
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

    // #9088: resolve story membership for every run this pass may emit, up
    // front, so a repo costs at most one identity probe and one batched
    // GraphQL request per cycle.
    let stories = ctx.repo_identity.map(|resolve| {
        let unseen: Vec<&RunJson> = runs
            .iter()
            .filter(|run| {
                run.is_completed() && !ledger.is_seen(&UnitKey::run(full, run.id, run.run_attempt))
            })
            .collect();
        let mut graphql_ok = !report.graphql_suppressed;
        let stories = RepoStories::prepare(
            resolve,
            api,
            full,
            &unseen,
            &mut report.summary.requests,
            &mut graphql_ok,
        );
        report.graphql_suppressed = !graphql_ok;
        stories
    });

    let mut newest = watermark;
    let mut oldest_incomplete: Option<DateTime<Utc>> = None;
    for run in &runs {
        newest = newest.max(run.created_at);
        if !run.is_completed() {
            hold(
                run.created_at,
                ctx.now,
                ctx.rescan_window,
                ctx.initial_lookback,
                &mut oldest_incomplete,
            );
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
            hold(
                run.created_at,
                ctx.now,
                ctx.rescan_window,
                ctx.initial_lookback,
                &mut oldest_incomplete,
            );
            continue;
        }
        let stitch = stories.as_ref().map(|stories| stories.decide(run));
        let story = match &stitch {
            Some(Stitch::Stitched(story)) => Some(story),
            _ => None,
        };
        // #9089: suite spans are resolved BEFORE the commit so they ride in
        // their job's own unit (see `suite_spans_for_run` on why, and on what
        // a failure here costs). A rate limit or a dead credential still
        // aborts the cycle; nothing else can stop the run from being emitted.
        let mut suite_spans = suite_spans_for_run(ctx, api, ledger, repo, run, &jobs, report)?;
        // Job units first, the run unit LAST: a torn commit can then only
        // lose the run unit, leaving the run "unseen" so the next poll
        // re-lists its jobs and commits exactly the missing ones.
        // #9089: the zero point every job's `dependency_wait_ms` is measured
        // from, taken once over the whole listing — a job resolved against a
        // partial listing would read its own creation as the run's first and
        // report no dependency wait at all.
        let baseline = JobCreationBaseline::of(&jobs);
        let mut drafts: Vec<UnitDraft> = jobs
            .iter()
            .map(|job| {
                let mut envelopes = job_envelopes(repo, run, job, baseline, &ctx.host_id);
                if let Some(story) = story {
                    story::stitch_job(&mut envelopes, story, run, job);
                }
                // Appended after stitching so the story pass never sees (and
                // never copies) a suite span into the story trace: a story
                // trace is a per-issue summary, and one leg's ~118 suite
                // spans would swamp it.
                if let Some(spans) = suite_spans.remove(&job.id) {
                    envelopes.extend(spans);
                }
                UnitDraft {
                    key: UnitKey::job(full, run.id, job.id, job.run_attempt),
                    envelopes,
                }
            })
            .filter(|draft| !ledger.is_seen(&draft.key))
            .collect();
        let mut run_unit = run_envelopes(repo, run, &ctx.host_id);
        if let Some(story) = story {
            story::stitch_run(&mut run_unit, story, run);
        }
        drafts.push(UnitDraft {
            key: run_key,
            envelopes: run_unit,
        });
        let committed = ledger.commit(drafts)?;
        emit(ledger, journal, &committed)?;
        for unit in &committed {
            if unit.key.job_id.is_some() {
                report.summary.jobs_emitted += 1;
            } else {
                report.summary.runs_emitted += 1;
                if let Some(stitch) = &stitch {
                    count_stitch(&mut report.summary, full, run, stitch);
                }
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

/// Count (and log) one emitted run's story decision (#9088). A run that is
/// not stitched is never silently dropped from the story: it is named here.
fn count_stitch(summary: &mut CycleSummary, repo: &str, run: &RunJson, stitch: &Stitch) {
    let (id, attempt) = (run.id, run.run_attempt);
    match stitch {
        Stitch::Stitched(story) => {
            summary.story_runs_stitched += 1;
            log::debug!("ci_telemetry: {repo} run {id}/{attempt} stitched into {}", story.story);
        }
        Stitch::NoCandidate => {
            summary.story_runs_no_candidate += 1;
            log::debug!("ci_telemetry: {repo} run {id}/{attempt} has no story candidate");
        }
        Stitch::Ambiguous(issues) => {
            summary.story_runs_ambiguous += 1;
            log::info!(
                "ci_telemetry: {repo} run {id}/{attempt} not stitched: several candidate issues \
                 {issues:?} (never guessed)"
            );
        }
        Stitch::Unresolved(reason) => {
            summary.story_runs_unresolved += 1;
            log::info!("ci_telemetry: {repo} run {id}/{attempt} not stitched: {reason}");
        }
    }
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

/// Unit coverage for [`hold`]'s bound, isolated from the fixture-driven
/// end-to-end coverage in [`super::tests::rerun_window`] (#8992).
#[cfg(test)]
mod hold_bound_tests {
    use super::*;

    #[test]
    fn a_run_exactly_at_the_bound_is_still_held() {
        let now: DateTime<Utc> = "2026-09-20T12:00:00Z".parse().unwrap();
        let rescan_window = Duration::hours(24);
        let initial_lookback = Duration::hours(24);
        let mut oldest = None;
        hold(now - rescan_window, now, rescan_window, initial_lookback, &mut oldest);
        assert_eq!(oldest, Some(now - rescan_window));
    }

    #[test]
    fn a_run_one_second_past_the_bound_is_not_held() {
        let now: DateTime<Utc> = "2026-09-20T12:00:00Z".parse().unwrap();
        let rescan_window = Duration::hours(24);
        let initial_lookback = Duration::hours(24);
        let mut oldest = None;
        hold(
            now - rescan_window - Duration::seconds(1),
            now,
            rescan_window,
            initial_lookback,
            &mut oldest,
        );
        assert_eq!(oldest, None);
    }

    /// The bound is the *smaller* of the two windows — same cap [`runs_floor`]
    /// applies — so a rescan window wider than the initial lookback does not
    /// widen the hold past what the listing floor could ever re-list anyway.
    #[test]
    fn the_bound_is_capped_by_the_smaller_of_the_two_windows() {
        let now: DateTime<Utc> = "2026-09-20T12:00:00Z".parse().unwrap();
        let mut oldest = None;
        hold(
            now - Duration::hours(7),
            now,
            Duration::hours(240),
            Duration::hours(6),
            &mut oldest,
        );
        assert_eq!(oldest, None, "7h old is past the 6h initial-lookback cap");
    }

    /// Several holds still track the *oldest* incomplete run within the
    /// bound — an already-tracked older run is never overwritten by a newer
    /// one, and a too-old run never displaces a within-bound one either.
    #[test]
    fn several_holds_track_the_oldest_within_bound_run() {
        let now: DateTime<Utc> = "2026-09-20T12:00:00Z".parse().unwrap();
        let rescan_window = Duration::hours(24);
        let initial_lookback = Duration::hours(24);
        let mut oldest = None;
        let newer = now - Duration::hours(1);
        let older = now - Duration::hours(2);
        let too_old = now - Duration::hours(25);
        hold(newer, now, rescan_window, initial_lookback, &mut oldest);
        hold(older, now, rescan_window, initial_lookback, &mut oldest);
        hold(too_old, now, rescan_window, initial_lookback, &mut oldest);
        assert_eq!(oldest, Some(older));
    }
}
