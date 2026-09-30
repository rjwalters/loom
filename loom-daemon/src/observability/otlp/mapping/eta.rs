//! OTLP mapping for `eta.estimate` / `eta.outcome` (#9289).
//!
//! Each is one log record. The **body** is the record's JSON — for an
//! estimate that is the whole `eta-explanation/v1` explanation — so ClickHouse
//! can `JSONExtract` any field of it, and no attribute policy bounds it. The
//! scalar summary rides as `loom.eta.*` attributes for cheap dashboards. The
//! record time is the estimate's `as_of`, or the outcome's `actual_at`.
//!
//! `loom.eta.version` / `loom.eta.revision` / `loom.eta.tree_state` are always
//! the **estimating** build, on both kinds, so accuracy groups by
//! `(heuristic, revision)` with one key. An outcome adds the observing build
//! as `loom.eta.outcome_version` / `_revision` / `_tree_state`. Each build's
//! `complete` flag rides as `loom.eta.provenance_complete` /
//! `loom.eta.outcome_provenance_complete`; accuracy queries keep only rows
//! where both are true.

use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv, kv_int, kv_string, nanos};
use crate::eta::Provenance;
use crate::telemetry::TelemetryRecord;

fn kv_bool(key: &str, value: bool) -> KeyValue {
    kv(
        key,
        AnyValue {
            value: Some(any_value::Value::BoolValue(value)),
        },
    )
}

fn kv_double(key: &str, value: f64) -> KeyValue {
    kv(
        key,
        AnyValue {
            value: Some(any_value::Value::DoubleValue(value)),
        },
    )
}

fn provenance(attributes: &mut Vec<KeyValue>, prefix: &str, loom: &Provenance) {
    attributes.push(kv_string(&format!("{prefix}version"), loom.version.clone()));
    attributes.push(kv_string(&format!("{prefix}revision"), loom.revision.clone()));
    attributes.push(kv_string(&format!("{prefix}tree_state"), loom.tree_state.clone()));
    attributes.push(kv_bool(&format!("{prefix}provenance_complete"), loom.complete));
}

fn opt_int(attributes: &mut Vec<KeyValue>, key: &str, value: Option<i64>) {
    if let Some(value) = value {
        attributes.push(kv_int(key, value));
    }
}

/// `(event_name, severity, record time, attributes, body)` for an ETA
/// record; `None` for every other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    match record {
        TelemetryRecord::EtaEstimate(r) => {
            let e = &r.explanation;
            let mut attributes = vec![
                kv_string("loom.repo", e.subject.repo.clone()),
                kv_int("loom.issue", i64::from(e.subject.issue)),
                kv_string("loom.story", e.subject.story.clone()),
                kv_string("loom.eta.estimate_id", e.estimate_id.clone()),
                kv_string("loom.eta.kind", e.kind.as_str()),
                kv_string("loom.eta.heuristic", e.heuristic.clone()),
                kv_bool("loom.eta.primary", r.primary),
                kv_string("loom.eta.trigger", r.trigger.as_str()),
            ];
            provenance(&mut attributes, "loom.eta.", &e.loom);
            opt_int(&mut attributes, "loom.pr_number", e.subject.pr_number.map(i64::from));
            if let Some(current) = &e.current_stage {
                attributes.push(kv_string("loom.eta.stage", current.stage.as_str()));
                attributes.push(kv_int("loom.eta.age_sec", current.age_sec));
            }
            if let Some(result) = &e.result {
                attributes.push(kv_int("loom.eta.p25_sec", result.p25_sec));
                attributes.push(kv_int("loom.eta.p50_sec", result.p50_sec));
                attributes.push(kv_int("loom.eta.p75_sec", result.p75_sec));
                attributes.push(kv_int("loom.eta.samples_min", result.samples_min as i64));
                attributes.push(kv_string(
                    "loom.eta.horizon_bucket",
                    crate::eta::score::bucket(result.p50_sec),
                ));
            }
            if let Some(reason) = e.no_estimate_reason {
                attributes.push(kv_string("loom.eta.no_estimate_reason", reason.as_str()));
            }
            let body = serde_json::to_string(e).unwrap_or_default();
            Some(("eta.estimate", SeverityNumber::Info, nanos(e.as_of), attributes, body))
        }
        TelemetryRecord::EtaOutcome(r) => {
            let e = &r.estimate;
            let s = &r.score;
            let mut attributes = vec![
                kv_string("loom.repo", e.repo.clone()),
                kv_int("loom.issue", i64::from(e.issue)),
                kv_string("loom.eta.estimate_id", e.estimate_id.clone()),
                kv_string("loom.eta.kind", e.kind.as_str()),
                kv_string("loom.eta.heuristic", e.heuristic.clone()),
                kv_string(
                    "loom.eta.outcome",
                    serde_json::to_value(s.outcome)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default(),
                ),
                kv_string("loom.eta.outcome_source", r.outcome_source.clone()),
                kv_int("loom.eta.lead_sec", s.lead_sec),
                kv_int("loom.eta.rework_rounds_actual", i64::from(s.rework_rounds_actual)),
            ];
            provenance(&mut attributes, "loom.eta.", &e.loom);
            provenance(&mut attributes, "loom.eta.outcome_", &r.loom);
            opt_int(&mut attributes, "loom.pr_number", e.pr_number.map(i64::from));
            opt_int(&mut attributes, "loom.eta.p50_sec", e.p50_sec);
            opt_int(&mut attributes, "loom.eta.error_sec", s.error_sec);
            opt_int(&mut attributes, "loom.eta.abs_error_sec", s.abs_error_sec);
            opt_int(&mut attributes, "loom.eta.outcome_resolution_sec", r.outcome_resolution_sec);
            opt_int(&mut attributes, "loom.eta.samples_min", s.samples_min.map(|n| n as i64));
            if let Some(covered) = s.covered {
                attributes.push(kv_bool("loom.eta.covered", covered));
            }
            if let Some(loss) = s.pinball_loss_sec {
                attributes.push(kv_double("loom.eta.pinball_loss_sec", loss));
            }
            for (key, value) in [
                ("loom.eta.horizon_bucket", s.horizon_bucket.as_deref()),
                ("loom.eta.age_bucket", s.age_bucket.as_deref()),
                ("loom.eta.stage", s.stage_at_estimate.map(|st| st.as_str())),
                ("loom.eta.result", r.result.as_deref()),
            ] {
                if let Some(value) = value {
                    attributes.push(kv_string(key, value));
                }
            }
            let body = serde_json::to_string(r).unwrap_or_default();
            Some(("eta.outcome", SeverityNumber::Info, nanos(s.actual_at), attributes, body))
        }
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::log_record_for;
    use crate::eta::emit::Trigger;
    use crate::eta::heuristics::LandV1;
    use crate::eta::{
        AgeSource, CurrentStage, CurrentState, EstimateInput, Heuristic, Provenance, Stage, Subject,
    };
    use crate::telemetry::kinds::eta::EtaEstimateRecord;
    use crate::telemetry::trace::story_context;
    use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
    use chrono::{TimeZone, Utc};
    use opentelemetry_proto::tonic::common::v1::any_value::Value;

    fn record() -> EtaEstimateRecord {
        let as_of = Utc.with_ymd_and_hms(2026, 9, 28, 12, 0, 0).unwrap();
        let input = EstimateInput {
            subject: Subject::new("rjwalters/loom", Some(1_073_994_527), 9289),
            as_of,
            current: CurrentState::At(CurrentStage {
                stage: Stage::ReviewWait,
                entered_at: Some(as_of),
                age_sec: 0,
                age_source: AgeSource::Bus,
                rework_rounds: 0,
            }),
            features: Default::default(),
            features_omitted: Vec::new(),
            provenance: Provenance {
                version: "0.19.476".to_string(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
                tree_state: "clean".to_string(),
                complete: true,
            },
            dispatch: None,
        };
        EtaEstimateRecord {
            trigger: Trigger::Transition,
            primary: true,
            explanation: Box::new(LandV1.estimate(&input, &Default::default())),
        }
    }

    fn attr(log: &opentelemetry_proto::tonic::logs::v1::LogRecord, key: &str) -> Option<Value> {
        log.attributes
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.clone())
    }

    #[test]
    fn estimate_log_body_is_the_explanation_in_the_story_trace() {
        let record = record();
        let mut envelope =
            TelemetryEnvelope::new("host", TelemetryRecord::EtaEstimate(record.clone()));
        let story = story_context(1_073_994_527, 9289).unwrap();
        envelope.trace_context = Some(story.clone());
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "eta.estimate");
        let Some(Value::StringValue(body)) = log.body.as_ref().and_then(|b| b.value.clone()) else {
            panic!("string body");
        };
        let parsed: crate::eta::Explanation = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, *record.explanation);
        assert_eq!(log.trace_id, story.trace_id.bytes());
        for key in [
            "loom.eta.estimate_id",
            "loom.eta.kind",
            "loom.eta.heuristic",
            "loom.eta.version",
            "loom.eta.revision",
            "loom.eta.tree_state",
            "loom.eta.stage",
            "loom.eta.age_sec",
            "loom.eta.no_estimate_reason",
            "loom.repo",
            "loom.issue",
        ] {
            assert!(attr(&log, key).is_some(), "{key}");
        }
        assert_eq!(
            attr(&log, "loom.eta.revision"),
            Some(Value::StringValue("9d8e226ce0123456789abcdef0123456789abcde".to_string()))
        );
        assert_eq!(log.time_unix_nano, super::nanos(record.explanation.as_of));
    }

    #[test]
    fn every_eta_attribute_is_allowlisted() {
        use crate::eta::score::{score, EstimateSummary, OutcomeKind};
        use crate::telemetry::kinds::eta::{EtaOutcomeRecord, ETA_LOG_ATTRIBUTE_KEYS};
        let estimate = record();
        let summary = EstimateSummary::of(&estimate.explanation);
        let mut summary_with_numbers = summary.clone();
        summary_with_numbers.p25_sec = Some(60);
        summary_with_numbers.p50_sec = Some(120);
        summary_with_numbers.p75_sec = Some(240);
        summary_with_numbers.pr_number = Some(9301);
        let at = summary.as_of + chrono::Duration::seconds(100);
        let outcome = EtaOutcomeRecord {
            score: score(&summary_with_numbers, OutcomeKind::Finished, at, &[]),
            estimate: summary_with_numbers,
            loom: estimate.explanation.loom.clone(),
            outcome_source: "sweep_terminal".to_string(),
            outcome_resolution_sec: Some(0),
            result: Some("exited".to_string()),
        };
        for record in [
            TelemetryRecord::EtaEstimate(estimate),
            TelemetryRecord::EtaOutcome(outcome),
        ] {
            let log = log_record_for(&TelemetryEnvelope::new("host", record)).unwrap();
            for kv in &log.attributes {
                assert!(
                    ETA_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str())
                        || ["loom.repo", "loom.issue", "loom.pr_number"].contains(&kv.key.as_str()),
                    "{} is not allowlisted",
                    kv.key
                );
            }
        }
    }
}
