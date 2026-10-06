//! The daily ETA refit (#10245): a dedicated task that writes one
//! `eta-fit/v1` coefficient file per UTC day from the cached fleet snapshots.
//!
//! - **Cadence.** First check [`FIRST_CHECK_DELAY`] after start, then every
//!   [`CHECK_INTERVAL`] (missed ticks skipped). Each check runs
//!   `fit::run::refit_if_due`, which fits only when today's file is missing
//!   and the snapshots are fresh (or the grace period is over), so this runs
//!   at most once per UTC day per host; a file the CLI wrote counts.
//! - **Isolation.** Each check runs in `spawn_blocking` behind `catch_unwind`:
//!   a failure or panic is logged at `warn` and retried on the next check. It
//!   never takes the ETA tracker's state lock, so it cannot stall estimates,
//!   and it is neither the collector's 5-minute ETA pass nor the work
//!   finder's tick.
//! - **No forge call.** It reads `.loom/state/eta/fleet/*.json`, which the
//!   ETA pass already reads, and writes one file under the ignored
//!   `.loom/state/eta/fit/`. It does not refresh the snapshots.
//! - **Either/or with the fleet refresh (#10263).** When
//!   `autonomous.eta.fleetRefresh.enabled` is on (the default), this task is
//!   not spawned: `observability::eta_fleet_refresh` runs the same
//!   `refit_if_due` at the end of each refresh cycle, right after refreshing
//!   the snapshots it reads.
//! - **Config.** `autonomous.eta.fit.enabled` / `LOOM_ETA_FIT_ENABLED`,
//!   default on, and only with `autonomous.eta.enabled`; read once at spawn.
//!   Default on because the task generates no work, is CPU-only, and is a
//!   no-op on a host with no fleet snapshot.

use super::queue::{DurableQueue, FanoutQueue, QueueSink};
use crate::eta::config::EtaConfig;
use crate::eta::fit::run::{FitCheckOutcome, FitReport, FitSkip};
use crate::eta::fit::{coeffs, run, Fitter};
use crate::eta::Provenance;
use crate::telemetry::kinds::eta_fit::{EtaFitRecord, EtaFitStage};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Delay before the first check, so a daemon start does not fit at once.
pub const FIRST_CHECK_DELAY: Duration = Duration::from_secs(10 * 60);

/// Interval between checks.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Longest `error` text an `eta.fit` record carries, in bytes.
pub const MAX_ERROR_BYTES: usize = 512;

/// Whether the daily refit runs under `config`.
#[must_use]
pub fn should_run(config: &EtaConfig) -> bool {
    config.enabled && config.fit_enabled
}

/// Which caller ran a fit check (the record's `trigger`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The end of a fleet refresh tick (`eta_fleet_refresh::after_cycle`).
    FleetRefresh,
    /// The standalone daily task.
    DailyTask,
}

impl Trigger {
    /// The record's `trigger` value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::FleetRefresh => "fleet_refresh",
            Trigger::DailyTask => "daily_task",
        }
    }
}

/// What one fit check did (#10391): [`run::FitCheckOutcome`] plus the
/// outcomes only a caller knows.
#[derive(Debug, Clone, PartialEq)]
pub enum FitCheck {
    /// `autonomous.eta.fit.enabled` is off.
    Disabled,
    /// A backfill is in progress and younger than
    /// `fleet_refresh::FIT_HOLD_HOURS`.
    Held,
    /// Nothing to do, and why.
    Skipped(FitSkip),
    /// Today's coefficient file was written.
    Wrote(Box<FitReport>),
    /// The fit failed; the next check retries.
    Failed(String),
    /// The check panicked.
    Panicked,
}

/// [`run::refit_check`] behind `catch_unwind`: a panic is an outcome.
#[must_use]
pub fn run_check(root: &Path, now: DateTime<Utc>, fitter: &Fitter) -> FitCheck {
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run::refit_check(root, now, fitter)
    }));
    match checked {
        Ok(FitCheckOutcome::Wrote(report)) => FitCheck::Wrote(report),
        Ok(FitCheckOutcome::Failed(e)) => FitCheck::Failed(format!("{e:#}")),
        Ok(FitCheckOutcome::Skipped(skip)) => FitCheck::Skipped(skip),
        Err(_) => FitCheck::Panicked,
    }
}

fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The `eta.fit` record for one fit check. Pure: everything is an argument.
#[must_use]
pub fn record_for(
    check: &FitCheck,
    trigger: Trigger,
    host_id: &str,
    started_at: DateTime<Utc>,
    duration_ms: u64,
    loom: &Provenance,
) -> EtaFitRecord {
    let at = crate::telemetry::trace::instant(started_at);
    let mut record = EtaFitRecord {
        check_id: crate::telemetry::trace::derived_hex(&["loom.eta.fit_check", host_id, &at], 16),
        trigger: trigger.as_str().to_string(),
        started_at,
        outcome: String::new(),
        skip_reason: None,
        error: None,
        fit_id: None,
        cutoff: None,
        window_start: None,
        window_days: None,
        data_through: None,
        snapshots: 0,
        snapshot_oldest_as_of: None,
        snapshot_newest_as_of: None,
        snapshot_as_of: Default::default(),
        stages: Default::default(),
        rows_total: None,
        rows_censored: None,
        rows_dropped_missing: None,
        rows_dropped_no_flags: None,
        rows_star_unknown: None,
        dwells: None,
        pruned: None,
        coeff_file: None,
        coeff_bytes: None,
        coeff_sha256: None,
        duration_ms,
        loom: loom.clone(),
    };
    match check {
        FitCheck::Disabled | FitCheck::Held | FitCheck::Skipped(_) => {
            record.outcome = "skipped".to_string();
            let reason = match check {
                FitCheck::Disabled => "disabled",
                FitCheck::Held => "held",
                FitCheck::Skipped(skip) => skip.reason(),
                _ => unreachable!("matched above"),
            };
            record.skip_reason = Some(reason.to_string());
            match check {
                FitCheck::Skipped(FitSkip::TodayExists { fit_id }) => {
                    let cutoff = run::midnight(started_at);
                    record.fit_id = Some(fit_id.clone());
                    record.cutoff = Some(cutoff);
                    record.coeff_file = Some(coeffs::path_for(cutoff));
                }
                FitCheck::Skipped(FitSkip::StaleBeforeGrace {
                    oldest_as_of,
                    newest_as_of,
                    snapshots,
                    ..
                }) => {
                    record.snapshots = count(*snapshots);
                    record.snapshot_oldest_as_of = Some(*oldest_as_of);
                    record.snapshot_newest_as_of = Some(*newest_as_of);
                }
                _ => {}
            }
        }
        FitCheck::Failed(e) => {
            record.outcome = "error".to_string();
            record.error = Some(truncate(e.clone(), MAX_ERROR_BYTES));
        }
        FitCheck::Panicked => record.outcome = "panic".to_string(),
        FitCheck::Wrote(report) => fill_written(&mut record, report),
    }
    record
}

fn fill_written(record: &mut EtaFitRecord, report: &FitReport) {
    record.outcome = "written".to_string();
    record.fit_id = Some(report.id.clone());
    record.cutoff = Some(report.as_of);
    record.window_start = Some(report.window_start);
    record.window_days = Some(report.window_days);
    record.data_through = Some(report.data_through);
    record.snapshots = count(report.snapshots);
    record.snapshot_oldest_as_of = report.snapshot_as_of.values().min().copied();
    record.snapshot_newest_as_of = report.snapshot_as_of.values().max().copied();
    record.snapshot_as_of = report.snapshot_as_of.clone();
    record.stages = report
        .stages
        .iter()
        .map(|(stage, s)| {
            (
                stage.as_str().to_string(),
                EtaFitStage {
                    rows: count(s.rows),
                    exits: count(s.exits),
                    exit_censored: count(s.exit_censored),
                    merge_events: count(s.merge_events),
                    merge_censored: count(s.rows.saturating_sub(s.merge_events)),
                    hazard: s.hazard,
                    aft: s.aft,
                },
            )
        })
        .collect();
    record.rows_total = Some(report.stages.values().map(|s| count(s.rows)).sum());
    record.rows_censored = Some(report.stages.values().map(|s| count(s.exit_censored)).sum());
    record.rows_dropped_missing = Some(count(report.rows_dropped_missing));
    record.rows_dropped_no_flags = Some(count(report.rows_dropped_no_flags));
    record.rows_star_unknown = Some(count(report.rows_star_unknown));
    record.dwells = Some(count(report.dwells));
    record.pruned = Some(count(report.pruned));
    record.coeff_file = report
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string);
    record.coeff_bytes = Some(count(report.coeff_bytes));
    record.coeff_sha256 = Some(report.coeff_sha256.clone());
}

/// Emit the `eta.fit` record for one fit check: persist it as the doctor's
/// `fit-check.json` and offer it to `sink`. A record whose provenance does not
/// validate is neither persisted nor emitted. Returns whether it was.
pub fn finish(
    root: &Path,
    sink: Option<&dyn QueueSink>,
    host_id: &str,
    trigger: Trigger,
    started_at: DateTime<Utc>,
    elapsed: Duration,
    check: &FitCheck,
) -> bool {
    let duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    let record =
        record_for(check, trigger, host_id, started_at, duration_ms, &Provenance::current());
    emit_record(root, sink, host_id, record)
}

/// Validate, persist and offer one built record ([`finish`]'s second half).
pub fn emit_record(
    root: &Path,
    sink: Option<&dyn QueueSink>,
    host_id: &str,
    record: EtaFitRecord,
) -> bool {
    if !record.has_provenance() {
        log::warn!("eta fit: dropped eta.fit record: invalid provenance");
        return false;
    }
    super::ops::eta_health::note_fit_check(
        record.started_at,
        record.skip_reason.as_deref().unwrap_or(&record.outcome),
    );
    if let Ok(body) = serde_json::to_string(&record) {
        crate::eta::health::write_fit_check(root, &body);
    }
    if let Some(sink) = sink {
        sink.offer(TelemetryEnvelope::new(host_id, TelemetryRecord::EtaFit(record)));
    }
    true
}

/// Start the daily refit for `workspace_root`. `None` when it is disabled.
pub fn spawn_task(
    workspace_root: PathBuf,
    otlp_queues: Vec<Arc<DurableQueue>>,
    host_id: String,
) -> Option<tokio::task::JoinHandle<()>> {
    let config = crate::eta::config::read(&workspace_root);
    if !should_run(&config) {
        log::info!(
            "eta fit: daily refit disabled (autonomous.eta.enabled={}, autonomous.eta.fit.enabled={})",
            config.enabled,
            config.fit_enabled
        );
        return None;
    }
    let fitter = run::current_fitter();
    let sink: Option<Arc<dyn QueueSink>> = (!otlp_queues.is_empty())
        .then(|| Arc::new(FanoutQueue::new(otlp_queues)) as Arc<dyn QueueSink>);
    Some(tokio::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        let mut interval = tokio::time::interval(CHECK_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            check(workspace_root.clone(), fitter.clone(), host_id.clone(), sink.clone()).await;
        }
    }))
}

/// One check, off the async workers and behind `catch_unwind`; always emits
/// its `eta.fit` record.
async fn check(root: PathBuf, fitter: Fitter, host_id: String, sink: Option<Arc<dyn QueueSink>>) {
    let started_at = Utc::now();
    let began = std::time::Instant::now();
    let blocking_root = root.clone();
    let outcome =
        tokio::task::spawn_blocking(move || run_check(&blocking_root, started_at, &fitter)).await;
    let outcome = outcome.unwrap_or(FitCheck::Panicked);
    match &outcome {
        FitCheck::Wrote(report) => log::info!(
            "eta fit: wrote {} (id={}, data_through={}, dwells={}, dropped missing={} no_flags={})",
            report.path.display(),
            report.id,
            report.data_through.to_rfc3339(),
            report.dwells,
            report.rows_dropped_missing,
            report.rows_dropped_no_flags
        ),
        FitCheck::Failed(e) => {
            log::warn!("eta fit: daily refit failed, retrying next check: {e}");
        }
        FitCheck::Panicked => {
            log::warn!("eta fit: daily refit panicked, retrying next check (ETA fit only)");
        }
        _ => {}
    }
    finish(
        &root,
        sink.as_deref(),
        &host_id,
        Trigger::DailyTask,
        started_at,
        began.elapsed(),
        &outcome,
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::eta::config::resolve;
    use chrono::{DateTime, TimeZone};
    use serde_json::json;

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 4, h, m, 0).unwrap()
    }

    /// `(name, now, today's file exists, snapshot as_ofs, want)`.
    type DueCase<'a> = (&'a str, DateTime<Utc>, bool, &'a [DateTime<Utc>], Option<DateTime<Utc>>);

    #[test]
    fn due_fits_once_a_day_on_fresh_snapshots_or_after_the_grace() {
        let today = at(0, 0);
        let fresh = [at(0, 30)];
        let stale = [today - chrono::Duration::hours(20)];
        let cases: [DueCase<'_>; 6] = [
            ("no snapshot", at(9, 0), false, &[], None),
            ("today's file present", at(9, 0), true, &fresh, None),
            ("fresh snapshot", at(1, 0), false, &fresh, Some(today)),
            ("stale before the grace", at(5, 59), false, &stale, None),
            ("stale at the grace", at(6, 0), false, &stale, Some(today)),
            ("stale after the grace", at(23, 0), false, &stale, Some(today)),
        ];
        for (name, now, exists, as_ofs, want) in cases {
            assert_eq!(run::due(now, exists, as_ofs), want, "{name}");
        }
        // One stale snapshot among fresh ones is a stale fleet.
        assert_eq!(run::due(at(1, 0), false, &[at(0, 30), stale[0]]), None);
        assert_eq!(run::midnight(at(23, 59)), today);
    }

    #[test]
    fn the_task_runs_by_default_and_each_switch_turns_it_off() {
        let no_env = |_: &str| None;
        assert!(should_run(&resolve(&json!({}), no_env)), "default on");

        let off = json!({"autonomous": {"eta": {"fit": {"enabled": false}}}});
        assert!(!should_run(&resolve(&off, no_env)));

        let env_off = |key: &str| (key == "LOOM_ETA_FIT_ENABLED").then(|| "0".to_string());
        assert!(!should_run(&resolve(&json!({}), env_off)));

        let eta_off = json!({"autonomous": {"eta": {"enabled": false}}});
        assert!(!should_run(&resolve(&eta_off, no_env)), "needs autonomous.eta.enabled");
    }

    #[test]
    fn the_fit_path_makes_no_forge_call_and_spawns_no_process() {
        // Production half of this file only: the test below names the needles.
        let this = include_str!("eta_fit.rs");
        let production = this.split("#[cfg(test)]").next().unwrap();
        let sources = [
            ("observability/eta_fit.rs", production),
            ("eta/fit/rows.rs", include_str!("../eta/fit/rows.rs")),
            ("eta/fit/run.rs", include_str!("../eta/fit/run.rs")),
            ("eta/flag_timeline.rs", include_str!("../eta/flag_timeline.rs")),
        ];
        let needles = [
            concat!("gh", "_invocation"),
            concat!("Gh", "Invocation"),
            concat!("forge", "_listing"),
            concat!("Command", "::new"),
        ];
        for (name, source) in sources {
            for needle in needles {
                assert!(!source.contains(needle), "{name} mentions `{needle}`");
            }
        }
    }
}

#[cfg(test)]
#[path = "eta_fit_tests.rs"]
mod record_tests;
