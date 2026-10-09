//! The nightly ETA backtest folds (#10492): a daily task on the explicit ETA
//! authority (#10918), else the declared fleet captain, that folds yesterday's walk-forward backtest for every registered
//! `land` heuristic and emits `eta.backtest.fold` / `eta.backtest.summary`.
//!
//! - **Cadence.** First check [`FIRST_CHECK_DELAY`] after start, then every
//!   [`CHECK_INTERVAL`] (missed ticks skipped). A check does work only when a
//!   day is due ([`crate::eta::nightly_folds::due_days`]: after 00:30 UTC, not
//!   already folded), so it folds once per UTC day; a restart does not repeat a
//!   day, and a few missed days are caught up, oldest first.
//! - **Authority-, else captain-gated.** Each check passes one gate first
//!   ([`gate_tick`], re-read every tick, so an edit needs no restart). When
//!   `fleet.etaAuthority` / `LOOM_ETA_AUTHORITY` names a host explicitly, that
//!   host arms the [`SINGLETON_JOB_NAME`] singleton job and folds, and every
//!   other host, the captain included, stands down (#10918,
//!   [`crate::eta::job_owner`]): the folds read the journals of the one host
//!   that emits ETAs, and it has the OTLP exporter. Otherwise the
//!   `fleet.captain` gate decides: the captain arms and folds; any other host
//!   stands down and emits nothing. With no captain declared **no host folds** (fail-closed,
//!   exactly like ci-telemetry's singleton gate): `fleet.captain` must name a
//!   host for the scoreboard to exist. The refusal is logged at `warn` and
//!   listed in `host.health.captainless_singleton_jobs` (#9014).
//! - **Durable delivery.** The saved `fold-<day>.json` is both the computed
//!   state and the recovery context: each tick queues every folded day that
//!   has no `delivered-<day>` marker ([`deliver_pending`]), with durable
//!   offers, and writes the marker only after all of that day's records were
//!   offered. A restart between fold and enqueue, a missing sink, or a failing
//!   queue leaves the day pending; a partial retry re-offers records with the
//!   same `fold_id` / `summary_id` for the collector to dedupe.
//! - **Isolation.** The fold runs in `spawn_blocking`; a failure or panic is
//!   logged at `warn` and retried on the next check. It never takes the ETA
//!   tracker's state lock.
//! - **No forge call.** It reads this host's journals, the cached fleet
//!   snapshots and, when present, an offline merged-PR cache. See
//!   [`crate::eta::nightly_folds`].
//! - **Retirement filing (#10525).** With `autonomous.eta.nightlyFolds.
//!   retirementFiling` (default **off**), each folding tick that has the folds
//!   saved then runs [`file_retirements`]: the same path as `loom-daemon eta
//!   retire --file` ([`crate::eta::retire_filing`]), which re-checks the
//!   same gate (the explicit ETA authority, else the captain), dedups against its ledger and the forge, and fails closed
//!   if the forge cannot be searched. It files issues only; it never
//!   unregisters a heuristic. An error is logged at `warn`, never fatal to the
//!   fold job, and retried next tick.
//! - **Config.** `autonomous.eta.nightlyFolds.enabled` /
//!   `LOOM_ETA_NIGHTLY_FOLDS_ENABLED`, default on (and only with
//!   `autonomous.eta.enabled`); read at spawn. Default on because the gate
//!   makes it a captain-only, CPU-only task: "default on for the captain".

use super::queue::{DurableQueue, FanoutQueue, QueueSink};
use crate::eta::config::EtaConfig;
use crate::eta::nightly_folds::{self, DayRecords, SINGLETON_JOB_NAME};
use crate::eta::Provenance;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Delay before the first check, so a daemon start does not fold at once.
pub const FIRST_CHECK_DELAY: Duration = Duration::from_secs(15 * 60);

/// Interval between checks.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Whether the nightly folds run under `config`.
#[must_use]
pub fn should_run(config: &EtaConfig) -> bool {
    config.enabled && config.nightly_folds_enabled
}

/// One check's fold decision (the explicit ETA authority, else
/// `fleet.captain`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoldGate {
    /// This host is the declared captain (no explicit ETA authority): armed,
    /// and it folds.
    Captain,
    /// This host is the explicit ETA authority (#10918): armed, and it folds.
    Authority,
    /// No `fleet.captain` and no explicit ETA authority declared: no fold
    /// (fail-closed, like ci-telemetry).
    NoCaptain,
    /// Another host folds (`owner`: the explicit ETA authority, else the
    /// captain): no fold, no record.
    StandDown { owner: String },
}

impl FoldGate {
    /// Whether this check folds.
    #[must_use]
    pub fn folds(&self) -> bool {
        matches!(self, Self::Captain | Self::Authority)
    }
}

/// Resolve this check's [`FoldGate`] and keep the armed-singleton registry in
/// step. Logs when the gate changes from `last`.
pub fn gate_tick(root: &Path, host_id: &str, last: &mut Option<FoldGate>) -> FoldGate {
    gate_tick_with(root, host_id, last, |k| std::env::var(k).ok())
}

/// [`gate_tick`] over an injected environment.
pub fn gate_tick_with(
    root: &Path,
    host_id: &str,
    last: &mut Option<FoldGate>,
    env: impl Fn(&str) -> Option<String>,
) -> FoldGate {
    use crate::eta::job_owner::{self, Owner};
    use crate::fleet_captain::{self as captain, CaptainGate};
    let gate = match job_owner::resolve_with(root, host_id, env) {
        Owner::Authority => {
            captain::record_owned_singleton_job(SINGLETON_JOB_NAME, true);
            FoldGate::Authority
        }
        Owner::AuthorityElsewhere { authority } => {
            captain::record_owned_singleton_job(SINGLETON_JOB_NAME, false);
            FoldGate::StandDown { owner: authority }
        }
        Owner::Captain(CaptainGate::Armed { captain: name }) => {
            match captain::arm_singleton_job(SINGLETON_JOB_NAME, root, host_id) {
                Ok(()) => FoldGate::Captain,
                // `fleet.captain` changed between the two reads: sit this
                // check out; the next one re-reads it.
                Err(_) => FoldGate::StandDown { owner: name },
            }
        }
        // Both refusals still go through `arm_singleton_job`, as ci-telemetry's
        // gate does: it records a no-captain refusal in the captainless
        // registry that `host.health.captainless_singleton_jobs` samples
        // (#9014) — this job fails closed, so it belongs there — and clears
        // that entry once a captain is declared. The trailing disarm covers a
        // `fleet.captain` edit between the two reads: this check never folds.
        Owner::Captain(CaptainGate::Refused { captain: name, .. }) => {
            let _ = captain::arm_singleton_job(SINGLETON_JOB_NAME, root, host_id);
            captain::disarm_singleton_job(SINGLETON_JOB_NAME);
            FoldGate::StandDown { owner: name }
        }
        Owner::Captain(CaptainGate::NoCaptainDeclared) => {
            let _ = captain::arm_singleton_job(SINGLETON_JOB_NAME, root, host_id);
            captain::disarm_singleton_job(SINGLETON_JOB_NAME);
            FoldGate::NoCaptain
        }
    };
    if last.as_ref() != Some(&gate) {
        match &gate {
            FoldGate::Captain => log::info!(
                "eta nightly folds: this host ({host_id}) is the fleet captain — it folds the \
                 nightly backtest (#10492)"
            ),
            FoldGate::Authority => log::info!(
                "eta nightly folds: this host ({host_id}) is the ETA authority \
                 (fleet.etaAuthority) — it folds the nightly backtest, whoever fleet.captain \
                 names (#10492, #10918)"
            ),
            FoldGate::NoCaptain => log::warn!(
                "eta nightly folds: no fleet.captain declared — no host folds the nightly \
                 backtest. Set `fleet.captain` in .loom/config.json to the host that should \
                 emit eta.backtest.* (#10492)"
            ),
            FoldGate::StandDown { owner } => log::info!(
                "eta nightly folds: standing down — {owner} folds (the ETA authority, else the \
                 fleet captain), this host is {host_id}: no folds and no eta.backtest.* records \
                 here (#10492, #10918)"
            ),
        }
        *last = Some(gate.clone());
    }
    gate
}

/// Durably offer every record of `day` to `sink`, folds, summaries, then stage-attribution rows, a
/// record whose provenance does not validate dropped (it can never be
/// delivered, so it must not hold the day open). Returns how many were
/// offered; the first failed offer stops and surfaces, leaving the day pending.
/// Records already offered stay queued, and a retry re-offers them: each
/// carries a stable id (`fold_id` / `summary_id` / `row_id`) for the collector to dedupe.
///
/// # Errors
/// A durable offer failed.
pub fn emit_day(sink: &dyn QueueSink, host_id: &str, day: &DayRecords) -> std::io::Result<usize> {
    let mut offered = 0;
    for fold in &day.folds {
        if !fold.has_provenance() {
            log::warn!("eta nightly folds: dropped eta.backtest.fold: invalid provenance");
            continue;
        }
        sink.offer_durable(TelemetryEnvelope::new(
            host_id,
            TelemetryRecord::EtaBacktestFold(fold.clone()),
        ))?;
        offered += 1;
    }
    for summary in &day.summaries {
        if !summary.has_provenance() {
            log::warn!("eta nightly folds: dropped eta.backtest.summary: invalid provenance");
            continue;
        }
        sink.offer_durable(TelemetryEnvelope::new(
            host_id,
            TelemetryRecord::EtaBacktestSummary(summary.clone()),
        ))?;
        offered += 1;
    }
    for row in &day.stage_attribution {
        if !row.has_provenance() {
            log::warn!("eta nightly folds: dropped eta.stage_attribution: invalid provenance");
            continue;
        }
        sink.offer_durable(TelemetryEnvelope::new(
            host_id,
            TelemetryRecord::EtaStageAttribution(row.clone()),
        ))?;
        offered += 1;
    }
    Ok(offered)
}

/// Queue every folded-but-undelivered day for `root`, oldest first, marking a
/// day delivered only once all of its records were durably offered. With no
/// `sink` nothing is consumed: the days stay pending until one is configured.
/// A failed offer or marker write stops the pass (order is kept) and the day
/// is retried next tick. Returns how many records were offered.
pub fn deliver_pending(root: &Path, sink: Option<&dyn QueueSink>, host_id: &str) -> usize {
    let Some(sink) = sink else {
        return 0;
    };
    let mut offered = 0;
    for (day, records) in nightly_folds::pending_delivery(root) {
        match emit_day(sink, host_id, &records) {
            Ok(n) => offered += n,
            Err(e) => {
                log::warn!("eta nightly folds: queuing {} failed, will retry: {e}", records.day);
                break;
            }
        }
        if let Err(e) = nightly_folds::mark_delivered(root, day) {
            log::warn!("eta nightly folds: marking {} delivered failed: {e}", records.day);
            break;
        }
    }
    offered
}

/// Fold whatever is due for `root`, with the production registry and the
/// configured `historyScope`.
#[must_use]
pub fn fold_due(root: &Path) -> Vec<DayRecords> {
    let eta = crate::eta::config::read(root);
    let scope = eta.history_scope;
    let fits = nightly_folds::FitArchive::load(root);
    nightly_folds::run_due(
        root,
        Utc::now(),
        eta.current_land.as_deref(),
        &|local| crate::eta::fleet::apply_scope(scope, root, local),
        &|before| fits.registry_before(before),
        &Provenance::current(),
    )
}

/// File retirement proposals from the saved folds (#10525). Logs the outcome;
/// an error is a `warn`, never fatal to the fold job.
pub fn file_retirements(root: &Path, host_id: &str) {
    match crate::eta::retire_filing::run_scheduled(root, host_id) {
        Ok(r) if r.filed.is_empty() => {}
        Ok(r) => log::info!("eta retirement filing: filed {:?}; already {:?}", r.filed, r.already),
        Err(e) => log::warn!("eta retirement filing: {e}"),
    }
}

/// Start the nightly folds for `workspace_root`. `None` when disabled.
pub fn spawn_task(
    workspace_root: PathBuf,
    otlp_queues: Vec<Arc<DurableQueue>>,
    host_id: String,
) -> Option<tokio::task::JoinHandle<()>> {
    let config = crate::eta::config::read(&workspace_root);
    if !should_run(&config) {
        log::info!(
            "eta nightly folds: disabled (autonomous.eta.enabled={}, \
             autonomous.eta.nightlyFolds.enabled={})",
            config.enabled,
            config.nightly_folds_enabled
        );
        return None;
    }
    let file_retirement_proposals = config.retirement_filing_enabled;
    let sink: Option<Arc<dyn QueueSink>> = (!otlp_queues.is_empty())
        .then(|| Arc::new(FanoutQueue::new(otlp_queues)) as Arc<dyn QueueSink>);
    Some(tokio::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        let mut interval = tokio::time::interval(CHECK_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_gate = None;
        loop {
            interval.tick().await;
            if !gate_tick(&workspace_root, &host_id, &mut last_gate).folds() {
                continue;
            }
            let root = workspace_root.clone();
            let tick_sink = sink.clone();
            let tick_host = host_id.clone();
            let tick = move || {
                let days = fold_due(&root);
                deliver_pending(&root, tick_sink.as_deref(), &tick_host);
                if file_retirement_proposals {
                    file_retirements(&root, &tick_host);
                }
                days
            };
            match tokio::task::spawn_blocking(tick).await {
                Ok(days) => {
                    for day in &days {
                        log::info!(
                            "eta nightly folds: folded {} ({} heuristic(s), {} challenger(s))",
                            day.day,
                            day.folds.len(),
                            day.summaries.len()
                        );
                    }
                }
                Err(_) => {
                    log::warn!("eta nightly folds: the fold panicked, retrying next check");
                }
            }
        }
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::eta::config::resolve;
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Collect(Mutex<Vec<TelemetryEnvelope>>);

    impl QueueSink for Collect {
        fn offer(&self, envelope: TelemetryEnvelope) {
            self.0.lock().unwrap().push(envelope);
        }

        fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
            self.offer(envelope);
            Ok(())
        }
    }

    #[test]
    fn the_task_runs_by_default_and_each_switch_turns_it_off() {
        let no_env = |_: &str| None;
        assert!(should_run(&resolve(&json!({}), no_env)), "default on");

        let off = json!({"autonomous": {"eta": {"nightlyFolds": {"enabled": false}}}});
        assert!(!should_run(&resolve(&off, no_env)));

        let env_off =
            |key: &str| (key == "LOOM_ETA_NIGHTLY_FOLDS_ENABLED").then(|| "0".to_string());
        assert!(!should_run(&resolve(&json!({}), env_off)), "env beats the default");
        let env_on = |key: &str| (key == "LOOM_ETA_NIGHTLY_FOLDS_ENABLED").then(|| "1".to_string());
        assert!(should_run(&resolve(&off, env_on)), "env beats config");

        let eta_off = json!({"autonomous": {"eta": {"enabled": false}}});
        assert!(!should_run(&resolve(&eta_off, no_env)), "needs autonomous.eta.enabled");
    }

    #[test]
    fn only_the_captain_folds() {
        assert!(FoldGate::Captain.folds());
        assert!(!FoldGate::NoCaptain.folds(), "fail-closed, like ci-telemetry");
        assert!(FoldGate::Authority.folds());
        assert!(!FoldGate::StandDown {
            owner: "other".into()
        }
        .folds());
    }

    /// #10532 review: a no-captain refusal reaches the #9014 captainless
    /// registry `host.health.captainless_singleton_jobs` samples, and leaves
    /// it once a captain (even another host) is declared.
    /// The armed and captainless registries are process-global, and both
    /// gate tests arm or disarm the same job: run them one at a time.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn a_no_captain_tick_lists_the_job_as_captainless() {
        use crate::fleet_captain::{armed_singleton_job_names, captainless_singleton_job_names};
        let _serial = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let job = SINGLETON_JOB_NAME.to_string();
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join(crate::config_resolver::LEGACY_CONFIG_REL);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        let mut last = None;

        std::fs::write(&config, "{}").unwrap();
        assert_eq!(gate_tick(root.path(), "loom-worker-1", &mut last), FoldGate::NoCaptain);
        assert!(captainless_singleton_job_names().contains(&job));
        assert!(!armed_singleton_job_names().contains(&job));

        std::fs::write(&config, r#"{"fleet": {"captain": "loom-worker-2"}}"#).unwrap();
        assert_eq!(
            gate_tick(root.path(), "loom-worker-1", &mut last),
            FoldGate::StandDown {
                owner: "loom-worker-2".into()
            }
        );
        assert!(!captainless_singleton_job_names().contains(&job));
        assert!(!armed_singleton_job_names().contains(&job));

        assert_eq!(gate_tick(root.path(), "loom-worker-2", &mut last), FoldGate::Captain);
        assert!(armed_singleton_job_names().contains(&job));
        assert!(!captainless_singleton_job_names().contains(&job));
        crate::fleet_captain::disarm_singleton_job(SINGLETON_JOB_NAME);
    }

    /// #10918: with `fleet.etaAuthority` naming a host other than the
    /// captain, the authority folds and the captain stands down; without it,
    /// the captain gate is unchanged.
    #[test]
    fn an_explicit_authority_folds_and_the_captain_stands_down() {
        use crate::fleet_captain::{armed_singleton_job_names, captainless_singleton_job_names};
        let _serial = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let job = SINGLETON_JOB_NAME.to_string();
        let no_env = |_: &str| None;
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join(crate::config_resolver::LEGACY_CONFIG_REL);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        let mut last = None;

        let both = r#"{"fleet": {"captain": "cap", "etaAuthority": "loom-worker-1"}}"#;
        std::fs::write(&config, both).unwrap();
        assert_eq!(
            gate_tick_with(root.path(), "cap", &mut last, no_env),
            FoldGate::StandDown {
                owner: "loom-worker-1".into()
            },
            "the captain does not fold"
        );
        assert!(!armed_singleton_job_names().contains(&job));
        assert!(!captainless_singleton_job_names().contains(&job), "it has an owner");
        let authority = gate_tick_with(root.path(), "loom-worker-1", &mut last, no_env);
        assert_eq!(authority, FoldGate::Authority);
        assert!(authority.folds());
        assert!(armed_singleton_job_names().contains(&job), "host.health lists it");

        // The key removed: the captain gate again, with no restart.
        std::fs::write(&config, r#"{"fleet": {"captain": "cap"}}"#).unwrap();
        assert_eq!(
            gate_tick_with(root.path(), "loom-worker-1", &mut last, no_env),
            FoldGate::StandDown {
                owner: "cap".into()
            }
        );
        assert!(!armed_singleton_job_names().contains(&job), "disarmed within one check");
        assert_eq!(gate_tick_with(root.path(), "cap", &mut last, no_env), FoldGate::Captain);
        crate::fleet_captain::disarm_singleton_job(SINGLETON_JOB_NAME);
    }

    /// Folds one day into `root` the way a tick does, returning its records.
    fn fold_one_day(root: &Path) -> DayRecords {
        let provenance = Provenance {
            version: "0.0.0".into(),
            revision: "a".repeat(40),
            tree_state: "clean".into(),
            complete: true,
        };
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-06T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut days = nightly_folds::run_due(
            root,
            now,
            None,
            &|h| h,
            &|_| crate::eta::Registry::builtin(),
            &provenance,
        );
        assert_eq!(days.len(), 1);
        days.remove(0)
    }

    /// A sink whose durable offers fail until `healthy` is set.
    struct Flaky {
        inner: Collect,
        healthy: std::sync::atomic::AtomicBool,
    }

    impl QueueSink for Flaky {
        fn offer(&self, envelope: TelemetryEnvelope) {
            self.inner.offer(envelope);
        }

        fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
            if self.healthy.load(std::sync::atomic::Ordering::SeqCst) {
                self.inner.offer(envelope);
                Ok(())
            } else {
                Err(std::io::Error::other("queue write failed"))
            }
        }
    }

    fn kinds(sink: &Collect) -> Vec<&'static str> {
        sink.0
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.record.kind())
            .collect()
    }

    #[test]
    fn a_folded_day_is_queued_folds_then_summaries_and_only_once() {
        let root = tempfile::tempdir().unwrap();
        let records = fold_one_day(root.path());
        let sink = Collect::default();
        let offered = deliver_pending(root.path(), Some(&sink), "host");
        assert_eq!(
            offered,
            records.folds.len() + records.summaries.len() + records.stage_attribution.len()
        );
        let kinds = kinds(&sink);
        assert_eq!(
            kinds.iter().filter(|k| **k == "eta.backtest.fold").count(),
            records.folds.len()
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == "eta.backtest.summary")
                .count(),
            records.summaries.len()
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == "eta.stage_attribution")
                .count(),
            records.stage_attribution.len()
        );
        let first_summary = kinds.iter().position(|k| *k == "eta.backtest.summary");
        let last_fold = kinds.iter().rposition(|k| *k == "eta.backtest.fold");
        if let (Some(summary), Some(fold)) = (first_summary, last_fold) {
            assert!(fold < summary, "folds before summaries");
        }
        assert_eq!(deliver_pending(root.path(), Some(&sink), "host"), 0, "delivered once");
    }

    /// #10532 review: a restart after `fold-<day>.json` is written but before
    /// the offer must not lose the day — `run_due` returns nothing for it, yet
    /// the saved records are still queued.
    #[test]
    fn a_restart_between_fold_and_enqueue_still_delivers_the_day() {
        let root = tempfile::tempdir().unwrap();
        let records = fold_one_day(root.path());
        let before = std::fs::read_to_string(nightly_folds::day_path(
            root.path(),
            chrono::NaiveDate::from_ymd_opt(2026, 10, 5).unwrap(),
        ))
        .unwrap();
        // "Restart": nothing was offered, and the fold is not recomputed.
        let sink = Collect::default();
        assert_eq!(
            deliver_pending(root.path(), Some(&sink), "host"),
            records.folds.len() + records.summaries.len() + records.stage_attribution.len()
        );
        let after = std::fs::read_to_string(nightly_folds::day_path(
            root.path(),
            chrono::NaiveDate::from_ymd_opt(2026, 10, 5).unwrap(),
        ))
        .unwrap();
        assert_eq!(before, after, "the computed fold is unchanged");
    }

    #[test]
    fn no_sink_retains_the_day_until_one_is_configured() {
        let root = tempfile::tempdir().unwrap();
        let records = fold_one_day(root.path());
        assert_eq!(deliver_pending(root.path(), None, "host"), 0);
        assert_eq!(nightly_folds::pending_delivery(root.path()).len(), 1);
        let sink = Collect::default();
        assert_eq!(
            deliver_pending(root.path(), Some(&sink), "host"),
            records.folds.len() + records.summaries.len() + records.stage_attribution.len()
        );
        assert!(nightly_folds::pending_delivery(root.path()).is_empty());
    }

    #[test]
    fn a_failing_sink_keeps_the_day_pending_until_it_recovers() {
        let root = tempfile::tempdir().unwrap();
        let records = fold_one_day(root.path());
        let sink = Flaky {
            inner: Collect::default(),
            healthy: std::sync::atomic::AtomicBool::new(false),
        };
        assert_eq!(deliver_pending(root.path(), Some(&sink), "host"), 0);
        assert_eq!(nightly_folds::pending_delivery(root.path()).len(), 1, "still pending");

        sink.healthy
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            deliver_pending(root.path(), Some(&sink), "host"),
            records.folds.len() + records.summaries.len() + records.stage_attribution.len()
        );
        assert!(nightly_folds::pending_delivery(root.path()).is_empty());
        assert_eq!(
            sink.inner.0.lock().unwrap().len(),
            records.folds.len() + records.summaries.len() + records.stage_attribution.len(),
            "every record queued exactly once after recovery"
        );
    }
}
