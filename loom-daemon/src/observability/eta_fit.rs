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
//!   `.loom/state/eta/fit/`. Refreshing the snapshots is an operator (or
//!   cron) step this task does not take.
//! - **Config.** `autonomous.eta.fit.enabled` / `LOOM_ETA_FIT_ENABLED`,
//!   default on, and only with `autonomous.eta.enabled`; read once at spawn.
//!   Default on because the task generates no work, is CPU-only, and is a
//!   no-op on a host with no fleet snapshot.

use crate::eta::config::EtaConfig;
use crate::eta::fit::{run, Fitter};
use chrono::Utc;
use std::path::PathBuf;
use std::time::Duration;

/// Delay before the first check, so a daemon start does not fit at once.
pub const FIRST_CHECK_DELAY: Duration = Duration::from_secs(10 * 60);

/// Interval between checks.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Whether the daily refit runs under `config`.
#[must_use]
pub fn should_run(config: &EtaConfig) -> bool {
    config.enabled && config.fit_enabled
}

/// Start the daily refit for `workspace_root`. `None` when it is disabled.
pub fn spawn_task(workspace_root: PathBuf) -> Option<tokio::task::JoinHandle<()>> {
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
    Some(tokio::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        let mut interval = tokio::time::interval(CHECK_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            check(workspace_root.clone(), fitter.clone()).await;
        }
    }))
}

/// One check, off the async workers and behind `catch_unwind`.
async fn check(root: PathBuf, fitter: Fitter) {
    let outcome = tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run::refit_if_due(&root, Utc::now(), &fitter)
        }))
    })
    .await;
    match outcome {
        Ok(Ok(None)) => {}
        Ok(Ok(Some(Ok(report)))) => log::info!(
            "eta fit: wrote {} (id={}, data_through={}, dwells={}, dropped missing={} no_flags={})",
            report.path.display(),
            report.id,
            report.data_through.to_rfc3339(),
            report.dwells,
            report.rows_dropped_missing,
            report.rows_dropped_no_flags
        ),
        Ok(Ok(Some(Err(e)))) => {
            log::warn!("eta fit: daily refit failed, retrying next check: {e:#}")
        }
        Ok(Err(_)) | Err(_) => {
            log::warn!("eta fit: daily refit panicked, retrying next check (ETA fit only)");
        }
    }
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
