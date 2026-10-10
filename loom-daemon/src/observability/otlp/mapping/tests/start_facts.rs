//! Issue #11280: `sweep.started`'s dispatch facts on the OTLP log, under the
//! keys and values `sweep.outcome` uses, equal to the native record's.

use super::*;
use crate::telemetry::{SweepStartFacts, SWEEP_START_FACT_LOG_ATTRIBUTE_KEYS};
use std::collections::BTreeMap;

fn facts() -> SweepStartFacts {
    SweepStartFacts {
        model: Some("opus".to_string()),
        effort: Some("high".to_string()),
        model_source: Some("explicit".to_string()),
        runtime: Some("codex".to_string()),
        attempt_index: Some(2),
        trigger: Some("retry_after_env_failure".to_string()),
        previous_sweep_id: Some("sweep-issue-4858-prior".to_string()),
    }
}

/// Each attribute as a JSON value, string or integer.
fn attributes(envelope: &TelemetryEnvelope) -> BTreeMap<String, serde_json::Value> {
    log_record_for(envelope)
        .unwrap()
        .attributes
        .into_iter()
        .filter_map(|kv| {
            let value = match kv.value?.value? {
                any_value::Value::StringValue(s) => serde_json::json!(s),
                any_value::Value::IntValue(i) => serde_json::json!(i),
                _ => return None,
            };
            Some((kv.key, value))
        })
        .collect()
}

#[test]
fn sweep_started_exports_its_facts_as_the_native_record_and_the_outcome_do() {
    let TelemetryRecord::SweepStarted(mut started) = sweep_started_envelope().record else {
        panic!("fixture is a sweep.started record")
    };
    started.facts = facts();
    let native = serde_json::to_value(&started).unwrap();
    let otlp = attributes(&envelope("host-a", TelemetryRecord::SweepStarted(started)));
    for key in SWEEP_START_FACT_LOG_ATTRIBUTE_KEYS {
        let field = key.strip_prefix("loom.").unwrap();
        assert!(!native[field].is_null(), "native sweep.started lacks {field}");
        assert_eq!(otlp.get(*key), Some(&native[field]), "OTLP {key} differs from native");
    }

    let TelemetryRecord::SweepOutcome(mut outcome) = sweep_outcome_envelope().record else {
        panic!("fixture is a sweep.outcome record")
    };
    let facts = facts();
    outcome.model = facts.model;
    outcome.effort = facts.effort;
    outcome
        .config
        .insert("runtime".to_string(), "codex".to_string());
    outcome.attempt_index = facts.attempt_index;
    outcome.trigger = facts.trigger;
    outcome.previous_sweep_id = facts.previous_sweep_id;
    let outcome = attributes(&envelope("host-a", TelemetryRecord::SweepOutcome(outcome)));
    for key in SWEEP_START_FACT_LOG_ATTRIBUTE_KEYS
        .iter()
        .filter(|k| **k != "loom.model_source")
    {
        assert_eq!(otlp.get(*key), outcome.get(*key), "{key} differs from sweep.outcome's");
    }
}

#[test]
fn unknown_facts_are_absent_attributes() {
    let TelemetryRecord::SweepStarted(mut started) = sweep_started_envelope().record else {
        panic!("fixture is a sweep.started record")
    };
    started.facts = SweepStartFacts::default();
    let otlp = attributes(&envelope("host-a", TelemetryRecord::SweepStarted(started)));
    for key in SWEEP_START_FACT_LOG_ATTRIBUTE_KEYS {
        assert!(!otlp.contains_key(*key), "{key} exported without a value");
    }
}
