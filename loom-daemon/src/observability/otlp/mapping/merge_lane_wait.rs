//! OTLP mapping for `merge.lane_wait` (#13029): one INFO log record per
//! enter/leave of a PR's wait behind another PR's merge, stamped at the tick
//! that observed it. The body is the record's JSON; the scalars ride as
//! `loom.merge_lane.*` attributes.

use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::session_output::LogParts;
use super::{kv_bool, kv_int, kv_string, nanos};
use crate::telemetry::TelemetryRecord;

pub(super) fn log_parts(record: &TelemetryRecord) -> Option<LogParts> {
    let TelemetryRecord::MergeLaneWait(r) = record else {
        return None;
    };
    if !r.loom.is_valid() {
        return None;
    }
    let mut attributes = vec![
        kv_string("loom.repo", &r.repo),
        kv_int("loom.pr_number", i64::from(r.pr)),
        kv_string("loom.merge_lane.phase", &r.phase),
        kv_string("loom.merge_lane.step", &r.step),
        kv_int("loom.merge_lane.blocker_pr", i64::from(r.blocker_pr)),
        kv_string("loom.merge_lane.blocker_source", &r.blocker_source),
        kv_string("loom.merge_lane.wait_started_at", r.wait_started_at.to_rfc3339()),
        kv_bool("loom.merge_lane.wait_started_at_lower_bound", r.wait_started_at_lower_bound),
        kv_string("loom.merge_lane.version", &r.loom.version),
        kv_string("loom.merge_lane.revision", &r.loom.revision),
        kv_bool("loom.merge_lane.provenance_complete", r.loom.complete),
    ];
    if let Some(left) = &r.left_as {
        attributes.push(kv_string("loom.merge_lane.left_as", left));
    }
    if let Some(secs) = r.wait_secs {
        attributes
            .push(kv_int("loom.merge_lane.wait_secs", i64::try_from(secs).unwrap_or(i64::MAX)));
    }
    Some(LogParts {
        event_name: "merge.lane_wait",
        severity: SeverityNumber::Info,
        source_at: nanos(r.observed_at),
        observed_at: nanos(r.observed_at),
        attributes,
        body: serde_json::to_string(r).ok()?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::super::log_record_for;
    use super::*;
    use crate::telemetry::kinds::merge_lane_wait::{
        MergeLaneWaitRecord, MERGE_LANE_WAIT_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::TelemetryEnvelope;

    fn record() -> MergeLaneWaitRecord {
        let at = Utc.with_ymd_and_hms(2026, 10, 21, 12, 5, 0).unwrap();
        MergeLaneWaitRecord {
            repo: "o/r".into(),
            pr: 7,
            phase: "leave".into(),
            step: "champion_merge".into(),
            blocker_pr: 5,
            blocker_source: "peer".into(),
            wait_started_at: at - chrono::Duration::seconds(240),
            wait_started_at_lower_bound: false,
            left_as: Some("admitted".into()),
            wait_secs: Some(240),
            observed_at: at,
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    #[test]
    fn every_attribute_is_declared() {
        let r = record();
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::MergeLaneWait(r.clone()));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "merge.lane_wait");
        assert_eq!(log.time_unix_nano, nanos(r.observed_at));
        let keys: Vec<&str> = log.attributes.iter().map(|kv| kv.key.as_str()).collect();
        for key in MERGE_LANE_WAIT_LOG_ATTRIBUTE_KEYS {
            assert!(keys.contains(key), "{key} is emitted");
        }
        for key in keys {
            assert!(
                MERGE_LANE_WAIT_LOG_ATTRIBUTE_KEYS.contains(&key)
                    || ["loom.record_id", "loom.kind", "loom.pr_number", "loom.repo"]
                        .contains(&key),
                "{key} is not allowlisted"
            );
        }
    }

    #[test]
    fn an_enter_carries_no_leave_fields() {
        let mut r = record();
        r.phase = "enter".into();
        r.left_as = None;
        r.wait_secs = None;
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::MergeLaneWait(r));
        let log = log_record_for(&envelope).unwrap();
        for key in ["loom.merge_lane.left_as", "loom.merge_lane.wait_secs"] {
            assert!(!log.attributes.iter().any(|kv| kv.key == key), "{key}");
        }
    }

    #[test]
    fn the_collector_keeps_every_merge_lane_attribute() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .unwrap();
        for key in MERGE_LANE_WAIT_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
