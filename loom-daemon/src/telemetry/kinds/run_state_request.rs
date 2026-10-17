//! `daemon.run_state_request` (#12546): one operator-control IPC request that
//! changes the daemon's run state (pause, drain, abort-drain, stop, restart),
//! recorded when the daemon *receives* it.
//!
//! # Why this kind exists
//!
//! `daemon.exit` could say only `requested_by: ipc-drain`, and a following
//! systemd SIGTERM was `unknown`: neither said **who** made the IPC call, so
//! an outage's author was recovered over chat. This record carries the caller
//! as the daemon can observe it from the socket (peer uid / pid, the caller's
//! process name and its parent chain) and the request id that `daemon.exit`
//! references.
//!
//! # Privacy
//!
//! Process **names only** (`/proc/<pid>/comm`), never argv or environment:
//! argv can hold secrets. Capture is best effort; an unreadable `/proc`, or a
//! platform with no peer credentials, leaves the fields absent and the verb
//! proceeds.
//!
//! **OTLP only.** The scalars ride as `loom.run_state.*` attributes
//! ([`RUN_STATE_REQUEST_LOG_ATTRIBUTE_KEYS`], which the collector's log
//! `keep_keys` must list; contract-tested). The body is the record's JSON.
//! **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;

/// Every log attribute key this kind exports besides `loom.kind` and
/// `loom.record_id` (already kept). The collector's `transform/privacy` log
/// `keep_keys` must list each.
pub const RUN_STATE_REQUEST_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.run_state.request_id",
    "loom.run_state.verb",
    "loom.run_state.flags",
    "loom.run_state.caller_pid",
    "loom.run_state.caller_uid",
    "loom.run_state.caller_comm",
    "loom.run_state.caller_chain",
    "loom.run_state.caller_internal",
    "loom.run_state.host_id",
];

/// The run-state IPC verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunStateVerb {
    /// `DrainAndRestartDaemon`.
    Drain,
    /// `AbortDrain`.
    AbortDrain,
    /// `PauseThenExit` (`restart --pause --then-exit`).
    PauseThenExit,
    /// `RestartDaemon`.
    Restart,
    /// `Shutdown`.
    Shutdown,
}

impl RunStateVerb {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Drain => "drain",
            Self::AbortDrain => "abort-drain",
            Self::PauseThenExit => "pause-then-exit",
            Self::Restart => "restart",
            Self::Shutdown => "shutdown",
        }
    }

    /// Whether the verb can end the daemon (so a later exit can be traced to
    /// it). `AbortDrain` cancels such a request instead.
    #[must_use]
    pub fn can_stop(self) -> bool {
        !matches!(self, Self::AbortDrain)
    }
}

/// The `daemon.run_state_request` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStateRequestRecord {
    /// Id the later `daemon.exit` quotes as `request_id`.
    pub request_id: String,
    /// The verb.
    pub verb: RunStateVerb,
    /// The verb's flags as `name=value`, plus the manifest id once known.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    /// The peer's pid, when the platform exposes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_pid: Option<i32>,
    /// The peer's uid, when the platform exposes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_uid: Option<u32>,
    /// The caller's process name (`comm`), when resolvable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_comm: Option<String>,
    /// Parent process names, nearest first (the caller's parent, then up).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caller_chain: Vec<String>,
    /// The caller is the daemon itself, its supervisor, or its parent: the
    /// auto-update or supervisor path rather than an operator.
    pub caller_internal: bool,
    /// This daemon's host id (`LOOM_HOST_ID` resolution).
    pub host_id: String,
    /// When the daemon received the request.
    pub observed_at: DateTime<Utc>,
    /// The emitting build (version and revision).
    pub loom: Provenance,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verb_serializes_as_its_wire_spelling() {
        for verb in [
            RunStateVerb::Drain,
            RunStateVerb::AbortDrain,
            RunStateVerb::PauseThenExit,
            RunStateVerb::Restart,
            RunStateVerb::Shutdown,
        ] {
            assert_eq!(serde_json::to_value(verb).unwrap(), verb.as_str());
        }
    }

    #[test]
    fn only_abort_drain_cannot_stop() {
        assert!(!RunStateVerb::AbortDrain.can_stop());
        assert!(RunStateVerb::Drain.can_stop() && RunStateVerb::Shutdown.can_stop());
    }

    #[test]
    fn collector_keeps_every_run_state_attribute() {
        const CONFIG: &str =
            include_str!("../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .expect("the transform/privacy log keep_keys line");
        for key in RUN_STATE_REQUEST_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
