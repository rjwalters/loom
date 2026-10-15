//! `daemon.exit` (#11839): why the daemon process ended.
//!
//! Emitted from the one funnel every deliberate exit passes through,
//! `observability::shutdown::exit`, before the exporter's final flush, so a
//! fleet outage is explainable from SigNoz alone: who asked for the stop (when
//! the daemon knows), whether a supervisor will relaunch it, and which sweeps
//! were in flight and what becomes of them.
//!
//! **Cannot emit:** SIGKILL, the OOM killer, a crash or a power loss run no
//! code. The absence of a `daemon.exit` between two `daemon.start` records for
//! a host is itself the "killed or crashed" signal. Also uncovered: a failed
//! startup (`EXIT_STARTUP_FAILURE`) and the CLI-side `std::process::exit`
//! calls, which are not the daemon process ending.
//!
//! **OTLP only**, on the daemon's direct exporter. The scalars ride as
//! `loom.daemon_exit.*` attributes ([`DAEMON_EXIT_LOG_ATTRIBUTE_KEYS`], which
//! the collector's log `keep_keys` must list; contract-tested). The body is the
//! record's JSON. **Provenance is required** and carries version / revision.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;

/// Every log attribute key this kind exports besides `loom.kind` and
/// `loom.record_id` (already kept). The collector's `transform/privacy` log
/// `keep_keys` must list each.
pub const DAEMON_EXIT_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.daemon_exit.signal",
    "loom.daemon_exit.exit_code",
    "loom.daemon_exit.requested_by",
    "loom.daemon_exit.relaunch_expected",
    "loom.daemon_exit.in_flight_count",
    "loom.daemon_exit.in_flight_sweeps",
    "loom.daemon_exit.in_flight_fate",
    "loom.daemon_exit.uptime_s",
    "loom.daemon_exit.supervisor",
];

/// Who asked for the exit, as far as the daemon can know. An external SIGTERM
/// (a `systemctl stop`, a `kill`) carries no sender identity in the signal
/// stream, so it is always [`Self::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequestedBy {
    /// The IPC `RestartDaemon` verb.
    IpcRestart,
    /// An operator drain (`restart --drain` / `drain --then-exit`).
    IpcDrain,
    /// The IPC `Shutdown` verb.
    IpcShutdown,
    /// Auto-update's pause-roll or its drain.
    AutoUpdate,
    /// The fleet run-state said `stopped` at boot.
    FleetState,
    /// The opt-in idle exit.
    IdleExit,
    /// An external signal, or anything else the daemon cannot attribute.
    Unknown,
}

impl RequestedBy {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IpcRestart => "ipc-restart",
            Self::IpcDrain => "ipc-drain",
            Self::IpcShutdown => "ipc-shutdown",
            Self::AutoUpdate => "auto-update",
            Self::FleetState => "fleet-state",
            Self::IdleExit => "idle-exit",
            Self::Unknown => "unknown",
        }
    }
}

/// What happens to the in-flight sweeps' processes when the daemon exits
/// (#5119): they die with the service cgroup under systemd, and reparent to
/// `pid 1` under launchd or when unsupervised. (A `systemd-run` scope child
/// survives; that imprecision, #6129, is known and not modelled here.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InFlightFate {
    /// Killed with the unit's cgroup (systemd).
    DieWithCgroup,
    /// Left running (launchd, or no supervisor).
    Survive,
}

impl InFlightFate {
    /// The fate under `supervisor` (`detect_supervisor()`'s value).
    #[must_use]
    pub fn for_supervisor(supervisor: Option<&str>) -> Self {
        if supervisor == Some("systemd") {
            Self::DieWithCgroup
        } else {
            Self::Survive
        }
    }

    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DieWithCgroup => "die_with_cgroup",
            Self::Survive => "survive",
        }
    }
}

/// The `daemon.exit` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonExitRecord {
    /// `SIGTERM` / `SIGINT` when a signal ended the daemon; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    /// The process exit code.
    pub exit_code: i32,
    /// Who asked for the exit.
    pub requested_by: RequestedBy,
    /// Whether a supervisor is expected to bring the daemon back.
    pub relaunch_expected: bool,
    /// `launchd`, `systemd`, or `none`.
    pub supervisor: String,
    /// Non-terminal sweep ids across every managed root at exit.
    pub in_flight_sweeps: Vec<String>,
    /// What becomes of those sweeps' processes.
    pub in_flight_fate: InFlightFate,
    /// Seconds since the daemon recorded its start; absent when it never did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_s: Option<u64>,
    /// When the exit began.
    pub observed_at: DateTime<Utc>,
    /// The emitting build (version and revision).
    pub loom: Provenance,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_by_serializes_as_its_wire_spelling() {
        for by in [
            RequestedBy::IpcRestart,
            RequestedBy::IpcDrain,
            RequestedBy::IpcShutdown,
            RequestedBy::AutoUpdate,
            RequestedBy::FleetState,
            RequestedBy::IdleExit,
            RequestedBy::Unknown,
        ] {
            assert_eq!(serde_json::to_value(by).unwrap(), by.as_str());
        }
    }

    #[test]
    fn fate_follows_the_supervisor() {
        assert_eq!(InFlightFate::for_supervisor(Some("systemd")), InFlightFate::DieWithCgroup);
        assert_eq!(InFlightFate::for_supervisor(Some("launchd")), InFlightFate::Survive);
        assert_eq!(InFlightFate::for_supervisor(None), InFlightFate::Survive);
        for fate in [InFlightFate::DieWithCgroup, InFlightFate::Survive] {
            assert_eq!(serde_json::to_value(fate).unwrap(), fate.as_str());
        }
    }

    #[test]
    fn collector_keeps_every_daemon_exit_attribute() {
        const CONFIG: &str =
            include_str!("../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .expect("the transform/privacy log keep_keys line");
        for key in DAEMON_EXIT_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
