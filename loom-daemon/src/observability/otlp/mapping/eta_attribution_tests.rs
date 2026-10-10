//! OTLP mapping for `eta.outcome.attribution` (#10929).

use super::super::log_record_for;
use crate::eta::stage_forecast::{Attribution, StageError};
use crate::eta::{Provenance, Stage};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
use chrono::{Duration, TimeZone, Utc};
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use std::collections::BTreeMap;

fn attr(log: &LogRecord, key: &str) -> Option<Value> {
    log.attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| v.value.clone())
}

fn provenance() -> Provenance {
    Provenance {
        version: "0.19.800".to_string(),
        revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
        tree_state: "clean".to_string(),
        complete: true,
    }
}

#[test]
fn an_attributed_outcome_exports_its_dominant_stage() {
    use crate::eta::heuristics::LandV1;
    use crate::eta::score::{score, EstimateSummary, OutcomeKind};
    use crate::eta::Heuristic;
    use crate::telemetry::kinds::eta::EtaOutcomeRecord;
    let input = crate::eta::EstimateInput {
        subject: crate::eta::Subject::new("rjwalters/loom", Some(1), 10929),
        as_of: Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap(),
        current: crate::eta::CurrentState::Refused(crate::eta::NoEstimateReason::UnknownStage),
        features: Default::default(),
        features_omitted: Vec::new(),
        provenance: provenance(),
        dispatch: None,
        stalls: Vec::new(),
        held: None,
        queue: Vec::new(),
        dependencies: None,
    };
    let explanation = LandV1.estimate(&input, &Default::default());
    let mut summary = EstimateSummary::of(&explanation);
    (summary.p25_sec, summary.p50_sec, summary.p75_sec) = (Some(600), Some(1200), Some(2400));
    let at = summary.as_of + Duration::seconds(4000);
    let attribution = Attribution {
        stages: BTreeMap::from([(
            Stage::ReviewWait,
            StageError {
                predicted_entry_sec: Some(0),
                predicted_dwell_sec: 1200,
                actual_entry_sec: Some(0),
                actual_dwell_sec: 4000,
                contribution_sec: 2800,
            },
        )]),
        unattributed_sec: 0,
        dominant_stage: Some(Stage::ReviewWait),
    };
    let record = EtaOutcomeRecord {
        score: score(&summary, OutcomeKind::Landed, at, &[]),
        estimate: summary,
        loom: provenance(),
        outcome_source: "pulls_read".to_string(),
        outcome_resolution_sec: Some(0),
        result: None,
        attribution: Some(attribution),
    };
    let log = log_record_for(&TelemetryEnvelope::new("host", TelemetryRecord::EtaOutcome(record)))
        .unwrap();
    assert_eq!(
        attr(&log, "loom.eta.attribution.dominant_stage"),
        Some(Value::StringValue("review_wait".to_string()))
    );
    assert_eq!(attr(&log, "loom.eta.attribution.unattributed_sec"), Some(Value::IntValue(0)));
}
