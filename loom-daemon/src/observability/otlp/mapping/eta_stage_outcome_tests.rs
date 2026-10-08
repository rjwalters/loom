//! OTLP mapping for `eta.stage_outcome` and `eta.outcome.attribution`
//! (#10929).

use super::super::log_record_for;
use crate::eta::stage_forecast::{Attribution, StageError};
use crate::eta::{Provenance, Stage};
use crate::telemetry::kinds::eta::ETA_LOG_ATTRIBUTE_KEYS;
use crate::telemetry::kinds::eta_stage_outcome::{EtaStageOutcomeRecord, StageExit};
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
fn a_stage_outcome_is_stamped_when_left_observed_at_the_pass_and_names_the_authority() {
    let left_at = Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap();
    let observed_at = left_at + Duration::seconds(300);
    let record = EtaStageOutcomeRecord {
        repo: "rjwalters/loom".to_string(),
        repo_id: Some(1_073_994_527),
        issue: 10929,
        pr_number: Some(10950),
        stage: Stage::ReviewWait,
        entered_at: Some(left_at - Duration::seconds(5400)),
        left_at,
        dwell_sec: Some(5400),
        exit: StageExit::Pass,
        next_stage: Some(Stage::MergeWait),
        event: "label.transition".to_string(),
        observed_at,
        resolution_sec: Some(300),
        open_estimates: 40,
        estimate_ids: vec!["0123456789abcdef".to_string()],
        loom: provenance(),
    };
    let envelope =
        TelemetryEnvelope::new("host-authority", TelemetryRecord::EtaStageOutcome(record.clone()));
    let log = log_record_for(&envelope).unwrap();
    assert_eq!(log.event_name, "eta.stage_outcome");
    assert_eq!(log.time_unix_nano, super::nanos(left_at), "event time");
    assert_eq!(log.observed_time_unix_nano, super::nanos(observed_at), "knowable-at");
    for kv in &log.attributes {
        assert!(
            ETA_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str())
                || [
                    "loom.repo",
                    "loom.record_id",
                    "loom.pr_number",
                    "loom.issue"
                ]
                .contains(&kv.key.as_str()),
            "{} is not allowlisted",
            kv.key
        );
    }
    for key in ETA_LOG_ATTRIBUTE_KEYS
        .iter()
        .filter(|k| k.starts_with("loom.eta.stage_outcome."))
    {
        assert!(attr(&log, key).is_some(), "{key} is emitted");
    }
    assert_eq!(
        attr(&log, "loom.eta.authority"),
        Some(Value::StringValue("host-authority".to_string())),
        "#10498: the emitting host is the authority"
    );
    assert_eq!(
        attr(&log, "loom.eta.stage_outcome.exit"),
        Some(Value::StringValue("pass".into()))
    );
    assert_eq!(attr(&log, "loom.eta.stage_outcome.dwell_sec"), Some(Value::IntValue(5400)));
    assert_eq!(attr(&log, "loom.issue"), Some(Value::IntValue(10929)));
    let Some(Value::StringValue(body)) = log.body.as_ref().and_then(|b| b.value.clone()) else {
        panic!("string body");
    };
    let parsed: EtaStageOutcomeRecord = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed, record);
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
