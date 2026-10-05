//! `loom.record_id` -- the deterministic id on every log record (Issue #10196).

use super::*;

fn record_id_attr(envelope: &TelemetryEnvelope) -> String {
    let record = log_record_for(envelope).expect("lifecycle kind maps to a log");
    let kv = record
        .attributes
        .iter()
        .find(|kv| kv.key == "loom.record_id")
        .expect("every log record carries loom.record_id");
    match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
        Some(any_value::Value::StringValue(s)) => s.clone(),
        other => panic!("record id must be a string, got {other:?}"),
    }
}

#[test]
fn same_envelope_yields_same_id_across_retries() {
    assert_eq!(
        record_id_attr(&sweep_started_envelope()),
        record_id_attr(&sweep_started_envelope())
    );
}

#[test]
fn distinct_natural_keys_yield_distinct_ids() {
    let a = sweep_started_envelope();
    let mut other_host = sweep_started_envelope();
    other_host.host_id = "host-z".to_string();
    let mut later = sweep_started_envelope();
    later.emitted_at += chrono::Duration::seconds(1);
    let phase = sweep_phase_envelope();
    let ids = [a, other_host, later, phase].map(|e| record_id_attr(&e));
    for (i, x) in ids.iter().enumerate() {
        for y in &ids[i + 1..] {
            assert_ne!(x, y);
        }
    }
}

#[test]
fn id_is_16_lowercase_hex_and_not_the_eta_estimate_id() {
    let id = record_id_attr(&sweep_outcome_envelope());
    assert_eq!(id.len(), 16);
    assert!(id
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
}

#[test]
fn every_lifecycle_log_kind_in_this_suite_carries_the_id() {
    for envelope in [
        sweep_started_envelope(),
        sweep_phase_envelope(),
        sweep_completed_envelope(SweepResult::Success),
        sweep_outcome_envelope(),
    ] {
        assert_eq!(record_id_attr(&envelope).len(), 16);
    }
}
