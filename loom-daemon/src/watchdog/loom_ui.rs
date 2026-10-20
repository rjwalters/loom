//! The watchdog's alerts (#11295, #12806): `daemon.down` once recovery is
//! exhausted, and `watchdog.peer-coord-degraded`, as `alert.transition` OTLP
//! records beside the forge issue each already files.
//!
//! These are the only alerts that still work when the daemon itself is dead,
//! so they come from this separate one-shot process. It cannot hand them to
//! the daemon's exporter, so it writes each record durably into its own OTLP
//! queue (tagged [`QUEUE_TAG`], under the marker's `repo_root`, exported with
//! that workspace's observability config) and flushes it once, bounded by
//! [`EXPORT_TIMEOUT`] ([`crate::observability::standalone_ops::deliver`]).
//!
//! A `<sentinel>.alert` file records an `open` and which exporters (by index)
//! it drained to (#12831). An `open` no exporter drained leaves a pending
//! record naming none, so it is tried again on the next tick. The `resolved`
//! goes only to the exporters that got the `open`, so an exporter that is down
//! cannot strand it, and it removes every still-queued `open` of the incident
//! (anywhere), so an `open` that never got out cannot go out after the outage
//! ended. The record is removed once the `resolved` drained, so a healthy
//! tick reads nothing and opens no connection. While an outage holds, the
//! `open` is re-sent once the file is [`crate::alerts::HEARTBEAT`] old, inside
//! the reader's 12 h heartbeat rule.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use crate::alerts::{self, Alert, Severity, State};
use crate::observability::standalone_ops::{self, Delivery, Report};
use crate::telemetry::TelemetryRecord;

/// Bound on one export: a tick must stay short.
pub const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
/// The watchdog's OTLP queue files are `<repo_root>/.loom/logs/<tag>-ops-queue.N.jsonl`.
pub const QUEUE_TAG: &str = "watchdog-alerts";
/// Rule for an outage recovery could not end.
pub const RULE_DAEMON_DOWN: &str = "daemon.down";
/// Rule for degraded peer-claim coordination.
pub const RULE_PEER_COORD: &str = "watchdog.peer-coord-degraded";

/// The "this open was exported" record kept beside `sentinel`.
#[must_use]
pub fn record_for(sentinel: &Path) -> PathBuf {
    let mut name = sentinel.as_os_str().to_os_string();
    name.push(".alert");
    PathBuf::from(name)
}

/// The record's pre-#12806 name (`<sentinel>.loom-ui`, written when loom-ui
/// answered the POST). Adopted once, so an outage open across the upgrade is
/// still resolved.
fn legacy_record_for(sentinel: &Path) -> PathBuf {
    let mut name = sentinel.as_os_str().to_os_string();
    name.push(".loom-ui");
    PathBuf::from(name)
}

fn adopt_legacy(sentinel: &Path) {
    let legacy = legacy_record_for(sentinel);
    if legacy.exists() {
        let record = record_for(sentinel);
        if record.exists() {
            let _ = std::fs::remove_file(&legacy);
        } else {
            let _ = std::fs::rename(&legacy, &record);
        }
    }
}

/// The exporters (indices) an `open` drained to, as `record` holds them:
/// `None` when there is no record, `Some(None)` for a pre-#12831 record (every
/// exporter), `Some(Some(list))` otherwise (empty: pending, nothing drained).
fn read_exporters(record: &Path) -> Option<Option<Vec<usize>>> {
    let text = std::fs::read_to_string(record).ok()?;
    let listed = text.lines().find_map(|l| l.strip_prefix("exporters"));
    Some(listed.map(|l| {
        l.split_whitespace()
            .filter_map(|n| n.parse().ok())
            .collect()
    }))
}

fn write_record(record: &Path, exporters: &[usize]) {
    if let Some(dir) = record.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let list: String = exporters.iter().map(|i| format!(" {i}")).collect();
    let _ =
        std::fs::write(record, format!("{}\nexporters{list}\n", chrono::Utc::now().to_rfc3339()));
}

/// Whether `alert` is owed at `now`: an `open` with no record, a pending one
/// (no exporter has it yet) or one a heartbeat old, or a `resolved` with a
/// record.
#[must_use]
pub fn owed(record: &Path, alert: &Alert, now: SystemTime) -> bool {
    match alert.state {
        State::Resolved => record.exists(),
        State::Open => {
            if read_exporters(record).is_some_and(|e| e.is_some_and(|e| e.is_empty())) {
                return true;
            }
            match std::fs::metadata(record).and_then(|m| m.modified()) {
                Ok(sent) => now
                    .duration_since(sent)
                    .is_ok_and(|age| age >= alerts::HEARTBEAT),
                Err(_) => true,
            }
        }
    }
}

/// Send `alert` if it is owed at `now`, keeping `record` in step. `deliver`
/// gets the exporters to target (`None`: all). `None` when nothing was owed.
pub fn sync(
    record: &Path,
    alert: &Alert,
    now: SystemTime,
    deliver: impl FnOnce(&Alert, Option<&[usize]>) -> Result<Report, String>,
) -> Option<Result<Delivery, String>> {
    if !owed(record, alert, now) {
        return None;
    }
    let prior = read_exporters(record);
    let outcome = match alert.state {
        State::Open => deliver(alert, None).inspect(|report| {
            let mut have: Vec<usize> = match &prior {
                Some(Some(held)) => held.clone(),
                // A pre-#12831 record: every exporter had it.
                Some(None) => report.pending.clone(),
                None => Vec::new(),
            };
            have.extend(&report.drained);
            have.sort_unstable();
            have.dedup();
            // Never narrow an existing record on a failed re-send.
            if !report.drained.is_empty() || prior.is_none() {
                write_record(record, &have);
            }
        }),
        State::Resolved => {
            let only = prior.flatten();
            deliver(alert, only.as_deref()).inspect(|report| {
                if report.pending.is_empty() {
                    let _ = std::fs::remove_file(record);
                } else {
                    write_record(record, &report.pending);
                }
            })
        }
    };
    Some(outcome.map(|report| report.delivery()))
}

/// The workspace this tick exports for, resolved at most once per process.
fn root() -> Option<&'static PathBuf> {
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let paths = super::config::Paths::from_env();
        super::marker::get_nonempty(&paths.marker, "repo_root")
            .map(PathBuf::from)
            .or_else(|| std::env::var("LOOM_WORKSPACE").ok().map(PathBuf::from))
            .or_else(|| std::env::current_dir().ok())
    })
    .as_ref()
}

/// Export `alert` through `root`'s OTLP config to the exporters in `only`
/// (all when `None`). Anything the incident left queued is replaced by it, so
/// an older `open` can never go out after a newer record.
fn export(root: &Path, alert: &Alert, only: Option<&[usize]>) -> Result<Report, String> {
    let config = crate::observability::read_config(root);
    let record = alerts::record(alert, &alerts::source_host(), chrono::Utc::now());
    let (rule, key) = (record.rule.clone(), record.key.clone());
    standalone_ops::deliver(
        &config,
        root,
        QUEUE_TAG,
        &TelemetryRecord::AlertTransition(record),
        only,
        |queued| matches!(queued, TelemetryRecord::AlertTransition(q) if q.rule == rule && q.key == key),
        EXPORT_TIMEOUT,
    )
}

fn send(sentinel: &Path, alert: &Alert) {
    adopt_legacy(sentinel);
    let record = record_for(sentinel);
    // Nothing owed: decided before resolving config, so a healthy tick reads
    // no config and opens no connection.
    if !owed(&record, alert, SystemTime::now()) {
        return;
    }
    let Some(root) = root() else { return };
    match sync(&record, alert, SystemTime::now(), |a, only| export(root, a, only)) {
        Some(Ok(Delivery::Queued)) => log::info!(
            "watchdog: alert {} {} queued, not yet exported everywhere; retried by a later tick",
            alert.rule,
            alert.state.as_str()
        ),
        Some(Err(why)) => {
            log::warn!("watchdog: alert {} {}: {why}", alert.rule, alert.state.as_str());
        }
        _ => {}
    }
}

/// `daemon.down` is open: recovery is exhausted.
pub fn daemon_down(escalation_sentinel: &Path, hostname: &str, detail: &str) {
    let alert = Alert::new(
        RULE_DAEMON_DOWN,
        "loom-daemon",
        super::escalate::title(hostname),
        Severity::Critical,
        State::Open,
    )
    .with_detail(detail);
    send(escalation_sentinel, &alert);
}

/// `daemon.down` is over (any tick that ends the outage episode).
pub fn daemon_up(escalation_sentinel: &Path) {
    let alert = Alert::new(
        RULE_DAEMON_DOWN,
        "loom-daemon",
        "loom-daemon is running again",
        Severity::Critical,
        State::Resolved,
    );
    send(escalation_sentinel, &alert);
}

/// Peer-claim coordination degraded (`open`) or green again (`resolved`).
pub fn peer_coord(peer_coord_sentinel: &Path, degraded: bool, hostname: &str, summary: &str) {
    let (state, title) = if degraded {
        (State::Open, format!("Peer-claim coordination is DEGRADED on {hostname}"))
    } else {
        (State::Resolved, format!("Peer-claim coordination recovered on {hostname}"))
    };
    let alert = Alert::new(RULE_PEER_COORD, "peer-claims", title, Severity::Critical, state)
        .with_detail(summary);
    send(peer_coord_sentinel, &alert);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn alert(state: State) -> Alert {
        Alert::new(RULE_DAEMON_DOWN, "loom-daemon", "down", Severity::Critical, state)
    }

    fn now() -> SystemTime {
        SystemTime::now()
    }

    fn report(drained: &[usize], pending: &[usize]) -> Result<Report, String> {
        Ok(Report {
            drained: drained.to_vec(),
            pending: pending.to_vec(),
        })
    }

    fn all_drained(_: &Alert, _: Option<&[usize]>) -> Result<Report, String> {
        report(&[0, 1], &[])
    }

    #[test]
    fn open_is_sent_once_then_holds_until_resolved() {
        let d = tempfile::tempdir().unwrap();
        let record = record_for(&d.path().join("sentinel"));
        let mut sent = Vec::new();
        let mut deliver = |a: &Alert, _: Option<&[usize]>| {
            sent.push(a.state);
            report(&[0, 1], &[])
        };
        assert_eq!(
            sync(&record, &alert(State::Open), now(), &mut deliver),
            Some(Ok(Delivery::Exported))
        );
        assert!(record.exists());
        // Later ticks of the same outage: nothing owed.
        assert_eq!(sync(&record, &alert(State::Open), now(), &mut deliver), None);
        assert_eq!(
            sync(&record, &alert(State::Resolved), now(), &mut deliver),
            Some(Ok(Delivery::Exported))
        );
        assert!(!record.exists());
        // Healthy ticks after that: nothing owed.
        assert_eq!(sync(&record, &alert(State::Resolved), now(), &mut deliver), None);
        assert_eq!(sent, vec![State::Open, State::Resolved]);
    }

    #[test]
    fn an_unexported_alert_is_retried_next_tick() {
        let d = tempfile::tempdir().unwrap();
        let record = record_for(&d.path().join("sentinel"));
        let queued = |_: &Alert, _: Option<&[usize]>| report(&[], &[0, 1]);
        assert!(sync(&record, &alert(State::Open), now(), queued).is_some());
        // Pending: no exporter has it, so it stays owed, and a resolve is owed
        // too (to purge what is queued) but goes to no exporter.
        assert!(owed(&record, &alert(State::Open), now()));
        let failed = |_: &Alert, _: Option<&[usize]>| Err("no OTLP exporter is configured".into());
        assert!(sync(&record, &alert(State::Open), now(), failed).is_some());
        assert!(owed(&record, &alert(State::Open), now()));
        assert!(sync(&record, &alert(State::Open), now(), all_drained).is_some());
        assert!(!owed(&record, &alert(State::Open), now()));
        assert!(sync(&record, &alert(State::Resolved), now(), queued).is_some());
        assert!(record.exists(), "a queued resolve stays owed");
    }

    #[test]
    fn a_resolve_goes_only_to_the_exporters_that_got_the_open() {
        let d = tempfile::tempdir().unwrap();
        let record = record_for(&d.path().join("sentinel"));
        // Exporter 1 is dead: only 0 got the open.
        let open = |_: &Alert, only: Option<&[usize]>| {
            assert_eq!(only, None);
            report(&[0], &[1])
        };
        assert_eq!(sync(&record, &alert(State::Open), now(), open), Some(Ok(Delivery::Queued)));
        assert!(record.exists());
        assert!(!owed(&record, &alert(State::Open), now()), "held until the heartbeat");
        let mut targeted = None;
        let resolve = |_: &Alert, only: Option<&[usize]>| {
            targeted = only.map(<[usize]>::to_vec);
            report(&[0], &[])
        };
        assert_eq!(
            sync(&record, &alert(State::Resolved), now(), resolve),
            Some(Ok(Delivery::Exported))
        );
        assert_eq!(targeted, Some(vec![0]), "the dead exporter is not a target");
        assert!(!record.exists(), "the resolve landed everywhere it was owed");
    }

    #[test]
    fn a_resolve_a_target_could_not_take_is_retried_there_alone() {
        let d = tempfile::tempdir().unwrap();
        let record = record_for(&d.path().join("sentinel"));
        assert!(sync(&record, &alert(State::Open), now(), all_drained).is_some());
        let partial = |_: &Alert, only: Option<&[usize]>| {
            assert_eq!(only, Some(&[0, 1][..]));
            report(&[0], &[1])
        };
        assert!(sync(&record, &alert(State::Resolved), now(), partial).is_some());
        let retry = |_: &Alert, only: Option<&[usize]>| {
            assert_eq!(only, Some(&[1][..]));
            report(&[1], &[])
        };
        assert!(sync(&record, &alert(State::Resolved), now(), retry).is_some());
        assert!(!record.exists());
    }

    #[test]
    fn a_resolve_after_an_open_nothing_took_still_purges_but_targets_no_one() {
        let d = tempfile::tempdir().unwrap();
        let record = record_for(&d.path().join("sentinel"));
        let down = |_: &Alert, _: Option<&[usize]>| report(&[], &[0, 1]);
        assert!(sync(&record, &alert(State::Open), now(), down).is_some());
        let mut targeted = None;
        let resolve = |_: &Alert, only: Option<&[usize]>| {
            targeted = only.map(<[usize]>::to_vec);
            report(&[], &[])
        };
        assert!(sync(&record, &alert(State::Resolved), now(), resolve).is_some());
        assert_eq!(targeted, Some(vec![]), "no exporter ever had the open");
        assert!(!record.exists());
    }

    #[test]
    fn a_pre_upgrade_record_resolves_everywhere() {
        let d = tempfile::tempdir().unwrap();
        let record = record_for(&d.path().join("sentinel"));
        std::fs::write(&record, "2026-10-19T00:00:00Z\n").unwrap();
        let mut targeted = Some(vec![9]);
        let resolve = |_: &Alert, only: Option<&[usize]>| {
            targeted = only.map(<[usize]>::to_vec);
            report(&[0, 1], &[])
        };
        assert!(sync(&record, &alert(State::Resolved), now(), resolve).is_some());
        assert_eq!(targeted, None);
    }

    #[test]
    fn a_held_open_is_re_sent_once_a_heartbeat_old() {
        let d = tempfile::tempdir().unwrap();
        let record = record_for(&d.path().join("sentinel"));
        assert!(sync(&record, &alert(State::Open), now(), all_drained).is_some());
        let later = now() + alerts::HEARTBEAT - Duration::from_secs(60);
        assert!(!owed(&record, &alert(State::Open), later));
        let due = now() + alerts::HEARTBEAT + Duration::from_secs(1);
        assert!(owed(&record, &alert(State::Open), due));
        assert!(alerts::HEARTBEAT <= Duration::from_secs(12 * 60 * 60));
    }

    #[test]
    fn a_pre_upgrade_record_is_adopted_so_the_outage_still_resolves() {
        let d = tempfile::tempdir().unwrap();
        let sentinel = d.path().join(".watchdog-outage-escalated");
        std::fs::write(legacy_record_for(&sentinel), "x\n").unwrap();
        adopt_legacy(&sentinel);
        assert!(record_for(&sentinel).exists());
        assert!(!legacy_record_for(&sentinel).exists());
        assert!(owed(&record_for(&sentinel), &alert(State::Resolved), now()));
    }

    #[test]
    fn the_record_sits_beside_its_sentinel() {
        assert_eq!(
            record_for(Path::new("/x/.watchdog-outage-escalated")),
            PathBuf::from("/x/.watchdog-outage-escalated.alert")
        );
    }

    #[test]
    fn observability_off_is_an_error_and_writes_no_queue() {
        let d = tempfile::tempdir().unwrap();
        let config = crate::observability::ObservabilityConfig {
            enabled: Some(false),
            ..Default::default()
        };
        let r = alerts::record(&alert(State::Open), "h", chrono::Utc::now());
        assert!(standalone_ops::deliver(
            &config,
            d.path(),
            QUEUE_TAG,
            &TelemetryRecord::AlertTransition(r),
            None,
            |_| false,
            EXPORT_TIMEOUT
        )
        .is_err());
        assert!(!d.path().join(".loom").exists());
        assert_eq!(
            standalone_ops::flush_pending(&config, d.path(), QUEUE_TAG, EXPORT_TIMEOUT),
            None
        );
    }
}
