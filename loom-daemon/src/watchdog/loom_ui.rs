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
//! A `<sentinel>.alert` file records an `open` that was exported: it is
//! written only once the export succeeded, so an unexported `open` is simply
//! tried again on the next tick (anything left in the queue goes first), and
//! removed once the `resolved` is exported, so a healthy tick reads nothing
//! and opens no connection. While an outage holds, the `open` is re-sent once
//! the file is [`crate::alerts::HEARTBEAT`] old, inside the reader's 12 h
//! heartbeat rule.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use crate::alerts::{self, Alert, Severity, State};
use crate::observability::standalone_ops::{self, Delivery};
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

/// Whether `alert` is owed at `now`: an `open` with no record or one a
/// heartbeat old, or a `resolved` with a record.
#[must_use]
pub fn owed(record: &Path, alert: &Alert, now: SystemTime) -> bool {
    match alert.state {
        State::Resolved => record.exists(),
        State::Open => match std::fs::metadata(record).and_then(|m| m.modified()) {
            Ok(sent) => now
                .duration_since(sent)
                .is_ok_and(|age| age >= alerts::HEARTBEAT),
            Err(_) => true,
        },
    }
}

/// Send `alert` if it is owed at `now`, keeping `record` in step. `None` when
/// nothing was owed.
pub fn sync(
    record: &Path,
    alert: &Alert,
    now: SystemTime,
    deliver: impl FnOnce(&Alert) -> Result<Delivery, String>,
) -> Option<Result<Delivery, String>> {
    if !owed(record, alert, now) {
        return None;
    }
    let outcome = deliver(alert);
    if outcome == Ok(Delivery::Exported) {
        if alert.state == State::Open {
            if let Some(dir) = record.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(record, format!("{}\n", chrono::Utc::now().to_rfc3339()));
        } else {
            let _ = std::fs::remove_file(record);
        }
    }
    Some(outcome)
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

/// Export `alert` through `root`'s OTLP config. Anything an earlier tick left
/// queued goes first; while that still cannot be exported nothing new is
/// queued behind it.
fn export(root: &Path, alert: &Alert) -> Result<Delivery, String> {
    let config = crate::observability::read_config(root);
    if standalone_ops::flush_pending(&config, root, QUEUE_TAG, EXPORT_TIMEOUT)
        == Some(Delivery::Queued)
    {
        return Ok(Delivery::Queued);
    }
    let record = alerts::record(alert, &alerts::source_host(), chrono::Utc::now());
    standalone_ops::deliver(
        &config,
        root,
        QUEUE_TAG,
        TelemetryRecord::AlertTransition(record),
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
    match sync(&record, alert, SystemTime::now(), |a| export(root, a)) {
        Some(Ok(Delivery::Queued)) => log::info!(
            "watchdog: alert {} {} queued, not yet exported; retried next tick",
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

    #[test]
    fn open_is_sent_once_then_holds_until_resolved() {
        let d = tempfile::tempdir().unwrap();
        let record = record_for(&d.path().join("sentinel"));
        let mut sent = Vec::new();
        let mut deliver = |a: &Alert| {
            sent.push(a.state);
            Ok(Delivery::Exported)
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
        let queued = |_: &Alert| Ok(Delivery::Queued);
        assert!(sync(&record, &alert(State::Open), now(), queued).is_some());
        assert!(!record.exists(), "a queued open stays owed");
        let failed = |_: &Alert| Err("no OTLP exporter is configured".to_string());
        assert!(sync(&record, &alert(State::Open), now(), failed).is_some());
        assert!(!record.exists());
        assert!(sync(&record, &alert(State::Open), now(), |_| Ok(Delivery::Exported)).is_some());
        assert!(record.exists());
        assert!(sync(&record, &alert(State::Resolved), now(), queued).is_some());
        assert!(record.exists(), "a queued resolve stays owed");
    }

    #[test]
    fn a_held_open_is_re_sent_once_a_heartbeat_old() {
        let d = tempfile::tempdir().unwrap();
        let record = record_for(&d.path().join("sentinel"));
        assert!(sync(&record, &alert(State::Open), now(), |_| Ok(Delivery::Exported)).is_some());
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
            TelemetryRecord::AlertTransition(r),
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
