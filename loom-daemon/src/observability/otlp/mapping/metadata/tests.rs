#![allow(clippy::unwrap_used)]
use super::*;
use crate::telemetry::{JudgeVerdict, TelemetryEnvelope, TelemetryRecord};
use opentelemetry_proto::tonic::logs::v1::LogRecord;
fn outcome_record() -> SweepOutcomeRecord {
    serde_json::from_value(serde_json::json!({
        "repo":"test/fixture", "issue":18, "sweep_id":"fixture-attempt-1",
        "total_duration_sec":3, "result":"failure"
    }))
    .unwrap()
}
fn map(record: TelemetryRecord) -> LogRecord {
    super::super::log_record_for(&TelemetryEnvelope::new("fixture-host", record)).unwrap()
}
fn attribute(log: &LogRecord, key: &str) -> Option<any_value::Value> {
    log.attributes
        .iter()
        .find(|v| v.key == key)
        .and_then(|v| v.value.as_ref())
        .and_then(|v| v.value.clone())
}
#[test]
fn outcomes_preserve_observed_zero_empty_history_and_missing_measurements() {
    let mut record = outcome_record();
    let missing = map(TelemetryRecord::SweepOutcome(record.clone()));
    for key in [
        "loom.doctor_cycles",
        "loom.judge_verdicts",
        "loom.tokens_in",
        "loom.tokens_by_model",
    ] {
        assert!(attribute(&missing, key).is_none(), "{key}");
    }
    record.doctor_cycles = Some(0);
    record.judge_verdicts = Some(vec![]);
    record.tokens_in = Some(0);
    record.tokens_by_model = Some(vec![ModelUsageTotals {
        model: "glm-5.3-flash".into(),
        ..Default::default()
    }]);
    let observed = map(TelemetryRecord::SweepOutcome(record));
    assert_eq!(attribute(&observed, "loom.doctor_cycles"), Some(any_value::Value::IntValue(0)));
    assert_eq!(attribute(&observed, "loom.tokens_in"), Some(any_value::Value::IntValue(0)));
    let Some(any_value::Value::ArrayValue(history)) = attribute(&observed, "loom.judge_verdicts")
    else {
        panic!("missing observed history")
    };
    assert!(history.values.is_empty());
    let Some(any_value::Value::ArrayValue(groups)) = attribute(&observed, "loom.tokens_by_model")
    else {
        panic!("missing observed usage")
    };
    let Some(any_value::Value::KvlistValue(group)) = &groups.values[0].value else {
        panic!("missing grouping dimensions")
    };
    assert_eq!(
        group
            .values
            .iter()
            .find(|v| v.key == "input")
            .unwrap()
            .value
            .as_ref()
            .unwrap()
            .value,
        Some(any_value::Value::IntValue(0))
    );
    assert!(attribute(&observed, "loom.tokens_out").is_none());
}
#[test]
fn repair_history_and_observed_model_groups_survive_without_creating_metrics() {
    let mut record = outcome_record();
    record.failure_class = Some("account-exhausted:model-credits-exhausted".into());
    record.doctor_cycles = Some(1);
    record.judge_verdicts = Some(vec![
        JudgeVerdict {
            attempt: 1,
            verdict: "fail".into(),
        },
        JudgeVerdict {
            attempt: 2,
            verdict: "pass".into(),
        },
    ]);
    record.models_used = Some(vec!["glm-5.3-flash".into(), "second-observed-model".into()]);
    let envelope = TelemetryEnvelope::new("fixture-host", TelemetryRecord::SweepOutcome(record));
    let log = super::super::log_record_for(&envelope).unwrap();
    assert_eq!(
        attribute(&log, "loom.failure_class"),
        Some(any_value::Value::StringValue(
            "account-exhausted:model-credits-exhausted".into()
        ))
    );
    let Some(any_value::Value::ArrayValue(history)) = attribute(&log, "loom.judge_verdicts") else {
        panic!("missing repair history")
    };
    assert_eq!(history.values.len(), 2);
    let Some(any_value::Value::KvlistValue(first)) = &history.values[0].value else {
        panic!("missing verdict")
    };
    assert_eq!(
        first.values[1].value.as_ref().unwrap().value,
        Some(any_value::Value::StringValue("fail".into()))
    );
    assert!(super::super::build_metrics_request(&[envelope]).is_none());
}
#[test]
fn freeform_details_and_config_credentials_never_enter_otlp() {
    let mut record = outcome_record();
    record.config.insert("runtime".into(), "pi".into());
    record
        .config
        .insert("token_account".into(), "SECRET_ACCOUNT_CONTENTS".into());
    record
        .config
        .insert("api_key".into(), "SECRET_API_KEY".into());
    record
        .config
        .insert("prompt".into(), "SECRET_PROMPT".into());
    let log = map(TelemetryRecord::SweepOutcome(record));
    let json = serde_json::to_string(&log).unwrap();
    assert!(!json.contains("SECRET"));
    assert_eq!(
        attribute(&log, "loom.runtime"),
        Some(any_value::Value::StringValue("pi".into()))
    );
    let role = serde_json::from_value(serde_json::json!({
        "repo":"test/fixture", "role":"judge", "started_at":"2026-09-21T00:00:00Z",
        "duration_sec":1,"result":"failure","detail":"SECRET_EXCEPTION_WITH_TOOL_OUTPUT"
    }))
    .unwrap();
    let log = map(TelemetryRecord::RoleTickOutcome(role));
    assert!(!serde_json::to_string(&log).unwrap().contains("SECRET"));
    assert!(attribute(&log, "loom.detail").is_none());
}
#[test]
fn oversized_or_invalid_usage_is_omitted_without_truncating_identity_or_fabricating_zero() {
    let mut record = outcome_record();
    record.model = Some("x".repeat(257));
    record.tokens_out = Some(u64::MAX);
    record.tokens_by_model = Some(vec![ModelUsageTotals {
        model: "glm-5.3-flash".into(),
        input: -1,
        ..Default::default()
    }]);
    let log = map(TelemetryRecord::SweepOutcome(record.clone()));
    for key in ["loom.model", "loom.tokens_out", "loom.tokens_by_model"] {
        assert!(attribute(&log, key).is_none());
    }
    assert!(!serde_json::to_string(&log)
        .unwrap()
        .contains(&"x".repeat(257)));
    record.tokens_by_model = Some(vec![ModelUsageTotals::default(); 65]);
    assert!(
        attribute(&map(TelemetryRecord::SweepOutcome(record)), "loom.tokens_by_model").is_none()
    );
}
