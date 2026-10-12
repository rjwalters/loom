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

/// The `daemon.start` record: the exporting build's identity, the supervisor,
/// the applied fleet-config SHA, and the pid.
#[must_use]
pub fn start_record(supervisor: &str) -> TelemetryRecord {
    let build = crate::telemetry::trace::provenance::daemon();
    TelemetryRecord::DaemonEvent(DaemonEventRecord {
        topic: TOPIC_START.to_string(),
        payload: serde_json::json!({
            "version": build.version,
            "revision": build.revision,
            "tree_state": build.tree_state,
            "supervisor": supervisor,
            "config_sha": crate::fleet_sync::applied_config_sha().as_resource_value(),
            "pid": std::process::id(),
        }),
    })
}

/// Enqueue exactly one `daemon.start` for `host_id` into `sink`. Called once
/// from [`spawn_task`](super::spawn_task) after at least one exporter was
/// planned — never on the disabled/under-configured paths, which return
/// before any queue exists.
pub(crate) fn emit_start(sink: &dyn QueueSink, host_id: &str) {
    sink.offer(TelemetryEnvelope::new(host_id.to_string(), start_record(&supervisor())));
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

    fn payload(record: &TelemetryRecord) -> (String, serde_json::Value) {
        let TelemetryRecord::DaemonEvent(inner) = record else {
            panic!("daemon.start must be a daemon.event record, got {record:?}")
        };
        (inner.topic.clone(), inner.payload.clone())
    }

    #[test]
    fn start_record_names_the_build_supervisor_config_sha_and_pid() {
        let (topic, p) = payload(&start_record("systemd"));
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
