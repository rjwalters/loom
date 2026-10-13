//! The executing host's build on the ETA stage-governing facts (#11765).
//!
//! `sweep.started`, `sweep.phase` and `stage.work_started` are emitted by the
//! host that runs the sweep phase, so they carry that host's `loom.version`,
//! full-SHA `loom.revision` and `host.id` as log attributes. A fit can then
//! join a stage stay to the Loom build that governed its dwell without a
//! resource-attribute join. The fleet is mixed-version, so this is per
//! emitting host, not a fleet scalar.
//!
//! Contrast `eta.stage_outcome`, whose `loom.eta.version` / `loom.eta.revision`
//! name the host *computing* the outcome, not the one that ran the sweep.
//!
//! Values come from the existing single source,
//! [`provenance::daemon`](crate::telemetry::trace::provenance::daemon) (what
//! every span reports), except `stage.work_started`, whose own typed
//! provenance (stamped at production) is used. `host.id` is the envelope's
//! `host_id`, the same string as the Resource's `host.id`.

use opentelemetry_proto::tonic::common::v1::KeyValue;

use super::kv_string;
use crate::telemetry::trace::provenance;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

/// The log attribute keys this module adds. The collector's
/// `transform/privacy` log `keep_keys` must list each one (contract-tested).
pub(in crate::observability::otlp) const EXECUTING_HOST_LOG_ATTRIBUTE_KEYS: &[&str] =
    &["loom.version", "loom.revision", "host.id"];

/// The executing-host attributes for `envelope`; empty for every kind other
/// than the three stage-governing facts.
pub(super) fn attributes(envelope: &TelemetryEnvelope) -> Vec<KeyValue> {
    let (version, revision) = match &envelope.record {
        TelemetryRecord::SweepStarted(_) | TelemetryRecord::SweepPhase(_) => {
            let build = provenance::daemon();
            (build.version.to_string(), build.revision.to_string())
        }
        TelemetryRecord::WorkStarted(r) => (r.loom.version.clone(), r.loom.revision.clone()),
        _ => return Vec::new(),
    };
    vec![
        kv_string("loom.version", version),
        kv_string("loom.revision", revision),
        kv_string("host.id", envelope.host_id.clone()),
    ]
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::log_record_for;
    use super::*;
    use crate::telemetry::SweepPhaseRecord;
    use opentelemetry_proto::tonic::common::v1::any_value::Value;

    fn attr(log: &opentelemetry_proto::tonic::logs::v1::LogRecord, key: &str) -> Option<Value> {
        log.attributes
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.clone())
    }

    #[test]
    fn sweep_phase_carries_the_emitting_hosts_build() {
        let record = TelemetryRecord::SweepPhase(SweepPhaseRecord {
            repo: "rjwalters/loom".into(),
            visibility: crate::telemetry::RepoVisibility::Public,
            issue: 1,
            sweep_id: "sweep-issue-1-0".into(),
            phase: "builder".into(),
            entered_at: chrono::Utc::now(),
        });
        let log = log_record_for(&TelemetryEnvelope::new("host-x", record)).unwrap();
        let build = provenance::daemon();
        assert_eq!(attr(&log, "loom.version"), Some(Value::StringValue(build.version.into())));
        assert_eq!(attr(&log, "loom.revision"), Some(Value::StringValue(build.revision.into())));
        assert_eq!(attr(&log, "host.id"), Some(Value::StringValue("host-x".into())));
    }

    #[test]
    fn collector_keeps_every_executing_host_attribute() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .expect("the transform/privacy log keep_keys line");
        for key in EXECUTING_HOST_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }

    #[test]
    fn other_kinds_are_untouched() {
        let record = TelemetryRecord::SweepPhase(SweepPhaseRecord {
            repo: "r/r".into(),
            visibility: crate::telemetry::RepoVisibility::Public,
            issue: 1,
            sweep_id: "s".into(),
            phase: "builder".into(),
            entered_at: chrono::Utc::now(),
        });
        let mut envelope = TelemetryEnvelope::new("h", record);
        assert_eq!(attributes(&envelope).len(), 3);
        envelope.record = TelemetryRecord::SweepIdentity(crate::telemetry::SweepIdentityRecord {
            repo: "r/r".into(),
            visibility: crate::telemetry::RepoVisibility::Public,
            issue: 1,
            sweep_id: "s".into(),
            runtime: None,
            provider: None,
            model: None,
        });
        assert!(attributes(&envelope).is_empty());
    }
}
