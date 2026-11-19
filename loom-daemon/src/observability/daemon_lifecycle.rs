//! Daemon lifecycle record: `daemon.start` (Issue #11197).
//!
//! One `daemon.event` log record enqueued when the exporter comes up — the
//! per-host "process started" marker that, alongside the resource attributes
//! `loom.fleet.config_sha` and `loom.daemon.revision`, makes a fleet-gitops
//! roll visible in SigNoz: query the DISTINCT resource `config_sha` per
//! `host.id` to see which hosts are on which store commit, and the
//! `daemon.start` record to see when each host's daemon last came up.
//!
//! Naming (`TOPIC_START`, `supervisor`, `start_record`) deliberately matches
//! draft PR #10065's `daemon_lifecycle.rs` so whichever of the two lands
//! first, the other rebases to a deletion rather than a conflict. What stays
//! with #10023 / #10065: `daemon.shutdown` and `daemon.heartbeat` — this
//! module emits exactly one record, once per process, on the export path.

use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::telemetry::kinds::daemon_exit::RequestedBy;
use crate::telemetry::{DaemonEventRecord, TelemetryEnvelope, TelemetryRecord};

use super::queue::QueueSink;

/// The one-shot startup topic.
pub const TOPIC_START: &str = "daemon.start";

/// The process supervisor this daemon runs under — `launchd`, `systemd`, or
/// `none` when unsupervised (the same detection `ipc.rs` uses to decide
/// whether a restart may exit at all).
#[must_use]
pub fn supervisor() -> String {
    crate::ipc::detect_supervisor().unwrap_or_else(|| "none".to_string())
}

/// The `daemon.start` payload fields that also ride as typed
/// `loom.payload.<field>` attributes. The whole payload outgrows the 256-byte
/// string bound once the #12942 fields are in, so `loom.payload` is dropped
/// for a real start record and these copies are what SigNoz reads. The
/// collector's `transform/privacy` allowlist keeps every one (contract-tested).
pub const START_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.payload.version",
    "loom.payload.revision",
    "loom.payload.tree_state",
    "loom.payload.supervisor",
    "loom.payload.config_sha",
    "loom.payload.pid",
    "loom.payload.start_time_unix_ms",
    "loom.payload.instance",
    "loom.payload.previous_version",
    "loom.payload.start_reason",
];

/// File name of the persisted lifecycle state under the auto-update state dir.
pub const STATE_FILE: &str = "daemon_lifecycle.json";

/// Why this process started (#12942), derived from what the previous process
/// left behind. A crash, SIGKILL or power loss leaves no exit cause, so it is
/// [`Self::Unknown`]: a `daemon.start` with no preceding `daemon.exit` is the
/// crash signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartReason {
    /// The previous process exited for an auto-update roll.
    Roll,
    /// The previous process exited for an IPC restart or an operator drain.
    Restart,
    /// No previous process is recorded on this host.
    Cold,
    /// A previous process is recorded but its exit cause is not.
    Unknown,
}

impl StartReason {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Roll => "roll",
            Self::Restart => "restart",
            Self::Cold => "cold",
            Self::Unknown => "unknown",
        }
    }
}

/// The on-disk lifecycle state: written at start (no exit cause) and rewritten
/// at every deliberate exit with the cause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleState {
    /// The crate version of the process that wrote it.
    pub version: String,
    /// The [`RequestedBy`] wire spelling of that process's exit, absent while
    /// it runs and after a crash.
    #[serde(default)]
    pub exit_cause: Option<String>,
}

/// What `daemon.start` adds to the build identity: this process's start time
/// and instance id, and the previous process's version and the start reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartContext {
    pub start_time_unix_ms: i64,
    pub instance: String,
    pub previous_version: Option<String>,
    pub reason: StartReason,
}

/// Where the lifecycle state lives: beside `auto_update_state.json`. `None`
/// when no state dir resolves.
#[must_use]
pub fn state_path() -> Option<PathBuf> {
    crate::auto_update::state_dir().map(|d| d.join(STATE_FILE))
}

/// Read the state at `path`; a missing, unreadable or corrupt file is `None`.
fn load_state(path: &Path) -> Option<LifecycleState> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Write `state` to `path`, best-effort: a failure costs a telemetry field,
/// never the daemon.
fn save_state(path: &Path, state: &LifecycleState) {
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(state).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, path)
    };
    if let Err(e) = write() {
        log::warn!("daemon lifecycle state not saved to {}: {e}", path.display());
    }
}

/// Map the previous process's recorded state to this process's start reason.
#[must_use]
pub fn reason_for(previous: Option<&LifecycleState>) -> StartReason {
    let Some(previous) = previous else {
        return StartReason::Cold;
    };
    let cause = previous.exit_cause.as_deref();
    if cause == Some(RequestedBy::AutoUpdate.as_str()) {
        StartReason::Roll
    } else if cause == Some(RequestedBy::IpcRestart.as_str())
        || cause == Some(RequestedBy::IpcDrain.as_str())
    {
        StartReason::Restart
    } else {
        StartReason::Unknown
    }
}

/// Build the [`StartContext`] from the state at `path`, then overwrite it with
/// this process's own (no exit cause yet).
fn begin_at(path: Option<&Path>, host_id: &str) -> StartContext {
    let previous = path.and_then(load_state);
    let now_ms = Utc::now().timestamp_millis();
    let ctx = StartContext {
        start_time_unix_ms: now_ms,
        instance: format!("{host_id}/{}", now_ms / 1000),
        previous_version: previous.as_ref().map(|p| p.version.clone()),
        reason: reason_for(previous.as_ref()),
    };
    if let Some(path) = path {
        save_state(
            path,
            &LifecycleState {
                version: env!("CARGO_PKG_VERSION").to_string(),
                exit_cause: None,
            },
        );
    }
    ctx
}

/// Persist the cause of this process's deliberate exit for the next start.
pub fn record_exit(cause: RequestedBy) {
    if let Some(path) = state_path() {
        record_exit_at(&path, cause);
    }
}

fn record_exit_at(path: &Path, cause: RequestedBy) {
    save_state(
        path,
        &LifecycleState {
            version: env!("CARGO_PKG_VERSION").to_string(),
            exit_cause: Some(cause.as_str().to_string()),
        },
    );
}

/// The `daemon.start` record: the exporting build's identity, the supervisor,
/// the applied fleet-config SHA, the pid, and the start context (#12942).
#[must_use]
pub fn start_record(supervisor: &str, ctx: &StartContext) -> TelemetryRecord {
    let build = crate::telemetry::trace::provenance::daemon();
    TelemetryRecord::DaemonEvent(DaemonEventRecord {
        topic: TOPIC_START.to_string(),
        payload: serde_json::json!({
            "version": build.version,
            "revision": build.revision,
            "tree_state": build.tree_state,
            "supervisor": supervisor,
            "config_sha": crate::fleet_sync::applied::applied_config_sha().as_resource_value(),
            "pid": std::process::id(),
            // #12942: per-process identity and why it started, so restarts
            // and crash loops are countable from `daemon.start` alone.
            "start_time_unix_ms": ctx.start_time_unix_ms,
            "instance": ctx.instance,
            "previous_version": ctx.previous_version,
            "start_reason": ctx.reason.as_str(),
            // #11934: how the work finder resolved at startup (null when the
            // daemon has not evaluated it). `forced_off: true` is a configured
            // dispatcher whose env switched its work finder off.
            "work_finder": crate::work_finder_guard::startup_verdict(),
        }),
    })
}

/// Enqueue exactly one `daemon.start` for `host_id` into `sink`. Called once
/// from [`spawn_task`](super::spawn_task) after at least one exporter was
/// planned — never on the disabled/under-configured paths, which return
/// before any queue exists.
pub(crate) fn emit_start(sink: &dyn QueueSink, host_id: &str) {
    // Unit tests must not touch a real daemon's state file under `~/.loom`.
    let path = if cfg!(test) { None } else { state_path() };
    let ctx = begin_at(path.as_deref(), host_id);
    sink.offer(TelemetryEnvelope::new(host_id.to_string(), start_record(&supervisor(), &ctx)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct CapturingSink(Mutex<Vec<TelemetryEnvelope>>);

    impl QueueSink for CapturingSink {
        fn offer(&self, envelope: TelemetryEnvelope) {
            self.0.lock().expect("sink lock").push(envelope);
        }
        fn offer_durable(&self, _envelope: TelemetryEnvelope) -> std::io::Result<()> {
            unreachable!("emit_start uses the best-effort offer")
        }
    }

    fn ctx(reason: StartReason) -> StartContext {
        StartContext {
            start_time_unix_ms: 1_700_000_000_123,
            instance: "host-a/1700000000".to_string(),
            previous_version: None,
            reason,
        }
    }

    fn payload(record: &TelemetryRecord) -> (String, serde_json::Value) {
        let TelemetryRecord::DaemonEvent(inner) = record else {
            panic!("daemon.start must be a daemon.event record, got {record:?}")
        };
        (inner.topic.clone(), inner.payload.clone())
    }

    #[test]
    fn start_record_names_the_build_supervisor_config_sha_and_pid() {
        let (topic, p) = payload(&start_record("systemd", &ctx(StartReason::Cold)));
        assert_eq!(topic, "daemon.start");
        let build = crate::telemetry::trace::provenance::daemon();
        assert_eq!(p["version"], build.version);
        assert_eq!(p["revision"], build.revision);
        assert_eq!(p["tree_state"], build.tree_state);
        assert_eq!(p["supervisor"], "systemd");
        // One of the three documented sentinels or a SHA — the same accessor
        // the resource reads, so the record and the resource can never
        // disagree about which store commit this host is on.
        let sha = p["config_sha"].as_str().expect("config_sha is a string");
        assert!(
            sha == "unknown" || sha == "none" || sha.len() == 40,
            "config_sha must be a SHA or an explicit sentinel, got {sha:?}"
        );
        assert_eq!(p["pid"], std::process::id());
        // #11934: present on every record; null until the daemon evaluated it.
        assert!(p.get("work_finder").is_some(), "daemon.start carries work_finder");
    }

    #[test]
    fn start_record_carries_the_identity_and_reason_fields() {
        let mut c = ctx(StartReason::Roll);
        c.previous_version = Some("0.19.1".to_string());
        let (_, p) = payload(&start_record("none", &c));
        assert_eq!(p["start_time_unix_ms"], 1_700_000_000_123_i64);
        assert_eq!(p["instance"], "host-a/1700000000");
        assert_eq!(p["previous_version"], "0.19.1");
        assert_eq!(p["start_reason"], "roll");
        let (_, p) = payload(&start_record("none", &ctx(StartReason::Cold)));
        assert!(p["previous_version"].is_null(), "null when unknown");
        for reason in [
            StartReason::Roll,
            StartReason::Restart,
            StartReason::Cold,
            StartReason::Unknown,
        ] {
            assert!(["roll", "restart", "cold", "unknown"].contains(&reason.as_str()));
        }
    }

    #[test]
    fn persisted_exit_cause_maps_to_the_start_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(STATE_FILE);

        // First boot on a clean host.
        let first = begin_at(Some(&path), "h");
        assert_eq!(first.reason, StartReason::Cold);
        assert_eq!(first.previous_version, None);
        assert!(first.instance.starts_with("h/"));

        // Predecessor never recorded an exit (crash / SIGKILL).
        let after_crash = begin_at(Some(&path), "h");
        assert_eq!(after_crash.reason, StartReason::Unknown);
        assert_eq!(after_crash.previous_version.as_deref(), Some(env!("CARGO_PKG_VERSION")));

        for (cause, want) in [
            (RequestedBy::AutoUpdate, StartReason::Roll),
            (RequestedBy::IpcRestart, StartReason::Restart),
            (RequestedBy::IpcDrain, StartReason::Restart),
            (RequestedBy::IpcShutdown, StartReason::Unknown),
            (RequestedBy::Unknown, StartReason::Unknown),
        ] {
            record_exit_at(&path, cause);
            assert_eq!(begin_at(Some(&path), "h").reason, want, "{cause:?}");
        }
    }

    #[test]
    fn a_corrupt_state_file_degrades_to_cold_and_is_rewritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(STATE_FILE);
        std::fs::write(&path, b"not json").expect("write");
        assert_eq!(begin_at(Some(&path), "h").reason, StartReason::Cold);
        assert!(load_state(&path).is_some(), "start rewrites a readable state");
        assert_eq!(begin_at(None, "h").reason, StartReason::Cold);
    }

    #[test]
    fn emit_start_offers_exactly_one_record() {
        let sink = CapturingSink(Mutex::new(Vec::new()));
        emit_start(&sink, "host-a");
        let offered = sink.0.lock().expect("sink lock");
        assert_eq!(offered.len(), 1, "exactly one daemon.start per call");
        assert_eq!(offered[0].host_id, "host-a");
        let (topic, _) = payload(&offered[0].record);
        assert_eq!(topic, "daemon.start");
    }

    #[test]
    fn supervisor_is_a_supervisor_or_none() {
        let value = supervisor();
        assert!(
            value == "none" || value == "launchd" || value == "systemd",
            "supervisor must be one of the three documented values, got {value:?}"
        );
    }
}
