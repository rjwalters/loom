//! OTLP mapping for the outcome facts `pr.resolved` (#10519) and
//! `eta.stage_outcome` (#10929), moved out of the ETA mapping by #11126.
//!
//! Each is one log record, stamped at its event time and observed at its
//! knowable-at time. The body is the record's JSON. The wire tags and every
//! `loom.eta.*` attribute key are unchanged from the ETA owner; the key list
//! is [`OUTCOME_FACT_LOG_ATTRIBUTE_KEYS`].
//!
//! `loom.eta.authority` on `eta.stage_outcome` is the emitting host. Nothing
//! is elected: every host that observes a transition emits it.
//!
//! [`OUTCOME_FACT_LOG_ATTRIBUTE_KEYS`]: crate::telemetry::kinds::stage_outcome::OUTCOME_FACT_LOG_ATTRIBUTE_KEYS

use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::session_output::LogParts;
use super::{kv, kv_int, kv_string, nanos};
use crate::telemetry::provenance::Provenance;
use crate::telemetry::trace::instant;
use crate::telemetry::TelemetryRecord;

fn provenance(attributes: &mut Vec<KeyValue>, loom: &Provenance) {
    attributes.push(kv_string("loom.eta.version", loom.version.clone()));
    attributes.push(kv_string("loom.eta.revision", loom.revision.clone()));
    attributes.push(kv_string("loom.eta.tree_state", loom.tree_state.clone()));
    attributes.push(kv(
        "loom.eta.provenance_complete",
        AnyValue {
            value: Some(any_value::Value::BoolValue(loom.complete)),
        },
    ));
}

fn opt_int(attributes: &mut Vec<KeyValue>, key: &str, value: Option<i64>) {
    if let Some(value) = value {
        attributes.push(kv_int(key, value));
    }
}

/// The log parts of an outcome fact, stamped at its event time and observed
/// at its knowable-at time; `None` for every other kind. `host_id` is the
/// emitting host.
pub(super) fn log_parts(record: &TelemetryRecord, host_id: &str) -> Option<LogParts> {
    match record {
        TelemetryRecord::PrResolved(r) => {
            let mut attributes = vec![
                kv_string("loom.repo", r.repo.clone()),
                kv_int("loom.pr_number", i64::from(r.pr_number)),
                kv_string("loom.eta.pr.state", r.state.as_str()),
                kv_string("loom.eta.pr.resolved_at", instant(r.resolved_at)),
                kv_string("loom.eta.pr.observed_at", instant(r.observed_at)),
                kv_int("loom.eta.pr.resolution_sec", r.resolution_sec),
            ];
            provenance(&mut attributes, &r.loom);
            opt_int(&mut attributes, "loom.issue", r.issue.map(i64::from));
            let body = serde_json::to_string(r).unwrap_or_default();
            Some(LogParts {
                event_name: "pr.resolved",
                severity: SeverityNumber::Info,
                source_at: nanos(r.resolved_at),
                observed_at: nanos(r.observed_at),
                attributes,
                body,
            })
        }
        TelemetryRecord::StageOutcome(r) => {
            let mut attributes = vec![
                kv_string("loom.repo", r.repo.clone()),
                kv_int("loom.issue", i64::from(r.issue)),
                kv_string("loom.eta.stage_outcome.stage", r.stage.as_str()),
                kv_string("loom.eta.stage_outcome.exit", r.exit.as_str()),
                kv_string("loom.eta.stage_outcome.left_at", instant(r.left_at)),
            ];
            provenance(&mut attributes, &r.loom);
            opt_int(&mut attributes, "loom.pr_number", r.pr_number.map(i64::from));
            opt_int(&mut attributes, "loom.eta.stage_outcome.dwell_sec", r.dwell_sec);
            for (key, value) in [
                (
                    "loom.eta.stage_outcome.next_stage",
                    r.next_stage.map(|s| s.as_str().to_string()),
                ),
                ("loom.eta.stage_outcome.entered_at", r.entered_at.map(instant)),
            ] {
                if let Some(value) = value {
                    attributes.push(kv_string(key, value));
                }
            }
            attributes.push(kv_string("loom.eta.authority", host_id.to_string()));
            let body = serde_json::to_string(r).unwrap_or_default();
            Some(LogParts {
                event_name: "eta.stage_outcome",
                severity: SeverityNumber::Info,
                source_at: nanos(r.left_at),
                observed_at: nanos(r.observed_at),
                attributes,
                body,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "outcome_facts_tests.rs"]
mod tests;
