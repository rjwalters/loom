//! OTLP mapping for `completion.narrated` (#13287): one INFO log record per
//! completion the narration sink built, stamped at the build. The body is the
//! record's JSON; the scalars ride as `loom.completion.*` attributes.

use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::session_output::LogParts;
use super::{kv_bool, kv_string, nanos};
use crate::telemetry::TelemetryRecord;

pub(super) fn log_parts(record: &TelemetryRecord) -> Option<LogParts> {
    let TelemetryRecord::CompletionNarrated(r) = record else {
        return None;
    };
    if !r.loom.is_valid() {
        return None;
    }
    let attributes = vec![
        kv_string("loom.repo", &r.repo),
        kv_string("loom.issue", r.issue.to_string()),
        kv_string("loom.completion.trigger", &r.trigger),
        kv_string("loom.completion.outcome", &r.outcome),
        kv_string("loom.completion.pr", r.pr.to_string()),
        kv_bool("loom.completion.tokens_present", r.tokens_present),
        kv_string("loom.completion.tokens_by_model_rows", r.tokens_by_model_rows.to_string()),
    ];
    Some(LogParts {
        event_name: "completion.narrated",
        severity: SeverityNumber::Info,
        source_at: nanos(r.narrated_at),
        observed_at: nanos(r.narrated_at),
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
    use crate::telemetry::kinds::completion_narrated::{
        CompletionNarratedRecord, COMPLETION_NARRATED_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::TelemetryEnvelope;

    fn record() -> CompletionNarratedRecord {
        CompletionNarratedRecord {
            repo: "acme/widgets".into(),
            issue: 7,
            pr: 9,
            trigger: "champion_merge_step".into(),
            outcome: "narrated".into(),
            tokens_present: false,
            tokens_by_model_rows: 0,
            narrated_at: Utc.with_ymd_and_hms(2026, 10, 29, 12, 0, 0).unwrap(),
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    #[test]
    fn every_attribute_is_declared_and_tokens_absence_is_explicit() {
        let envelope =
            TelemetryEnvelope::new("host-1", TelemetryRecord::CompletionNarrated(record()));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "completion.narrated");
        let keys: Vec<&str> = log.attributes.iter().map(|kv| kv.key.as_str()).collect();
        for key in COMPLETION_NARRATED_LOG_ATTRIBUTE_KEYS {
            assert!(keys.contains(key), "{key} is emitted");
        }
        for key in keys {
            assert!(
                COMPLETION_NARRATED_LOG_ATTRIBUTE_KEYS.contains(&key)
                    || ["loom.record_id", "loom.kind", "loom.repo", "loom.issue"].contains(&key),
                "{key} is not allowlisted"
            );
        }
        let present = log
            .attributes
            .iter()
            .find(|kv| kv.key == "loom.completion.tokens_present")
            .unwrap();
        assert!(format!("{present:?}").contains("false"));
    }

    #[test]
    fn the_collector_keeps_every_completion_attribute() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .unwrap();
        for key in COMPLETION_NARRATED_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
