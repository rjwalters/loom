//! `sweep.outcome`'s attempt-lineage wire contract (Issue #9444), in its own
//! sibling module for the same file-size-ratchet reason `tests/complexity.rs`
//! is: the parent test module is already large, and the rule is to add a
//! sibling rather than grow it.
//!
//! The classifier's own exhaustive tests are pure and live with it, in
//! [`crate::telemetry::lineage`]; the derivation's are in
//! `sweep_registry::outcome_journal::{lineage,rework}`. What is proven HERE is
//! only what crosses the wire: absent vs. observed-empty, and that a record
//! written by a daemon predating these fields still decodes.

use super::*;

/// The all-unobserved baseline record — every lineage field absent.
fn base() -> SweepOutcomeRecord {
    SweepOutcomeRecord {
        repo: Some("rjwalters/loom".to_string()),
        repo_unresolved: false,
        visibility: RepoVisibility::Private,
        issue: 9444,
        sweep_id: "sweep-issue-9444-0".to_string(),
        model: None,
        effort: None,
        config: std::collections::BTreeMap::new(),
        phase_durations: Vec::new(),
        total_duration_sec: 512,
        result: SweepResult::Failure,
        disposition: SweepDisposition::EnvFailure,
        pr_number: None,
        tokens_in: None,
        tokens_out: None,
        lines_added: None,
        lines_deleted: None,
        tokens_by_model: None,
        tokens_unattributed: None,
        failure_class: Some("unclassified:spawn-death".to_string()),
        models_used: None,
        doctor_cycles: None,
        judge_verdicts: None,
        runtime: None,
        provider: None,
        profile: None,
        complexity: None,
        attempt_index: None,
        previous_sweep_id: None,
        trigger: None,
        rework_events: None,
    }
}

/// Nobody counted: all four keys are absent from the wire. In particular
/// `attempt_index` is NOT a fabricated `1` and `rework_events` is NOT `[]` —
/// both of those are load-bearing observations that must not be manufactured.
#[test]
fn an_underived_lineage_omits_all_four_keys() {
    let value = serde_json::to_value(base()).unwrap();
    for field in [
        "attempt_index",
        "previous_sweep_id",
        "trigger",
        "rework_events",
    ] {
        assert!(
            value.get(field).is_none(),
            "underived {field:?} must be omitted, not null/zero: {value}"
        );
    }
}

/// The clean-landing shape: the timeline WAS read and carried no rework, so
/// `rework_events` is an empty array on the wire — a measurement, not a
/// default. A consumer that coerces absent to `[]` counts an unobserved sweep
/// as a rework-free one, which is the exact error the distinction prevents.
#[test]
fn an_observed_clean_landing_is_an_empty_array_not_an_absent_key() {
    let value = serde_json::to_value(SweepOutcomeRecord {
        attempt_index: Some(1),
        trigger: Some(SweepTrigger::First),
        rework_events: Some(Vec::new()),
        ..base()
    })
    .unwrap();
    assert_eq!(
        value
            .get("rework_events")
            .and_then(serde_json::Value::as_array),
        Some(&vec![]),
        "an observed-but-rework-free PR must be an empty list: {value}"
    );
    assert_eq!(value["trigger"], "first");
    assert_eq!(value["attempt_index"], 1);
    assert!(
        value.get("previous_sweep_id").is_none(),
        "attempt 1 has no predecessor to name: {value}"
    );
}

/// A fully-observed retry round-trips, classification inline on each event.
#[test]
fn a_fully_observed_lineage_round_trips() {
    let record = SweepOutcomeRecord {
        attempt_index: Some(3),
        previous_sweep_id: Some("sweep-issue-9444-1".to_string()),
        trigger: Some(SweepTrigger::RetryAfterEnvFailure),
        rework_events: Some(vec![
            ReworkEvent::new(ReworkKind::Rejudge, Some("loom:changes-requested".into()), Some(900)),
            ReworkEvent::new(ReworkKind::MergeConflict, Some("loom:merge-conflict".into()), None),
        ]),
        ..base()
    };
    let value = serde_json::to_value(&record).unwrap();
    assert_eq!(value["attempt_index"], 3);
    assert_eq!(value["previous_sweep_id"], "sweep-issue-9444-1");
    assert_eq!(value["trigger"], "retry_after_env_failure");
    assert_eq!(value["rework_events"][0]["kind"], "rejudge");
    assert_eq!(value["rework_events"][0]["classification"], "substantive");
    assert_eq!(value["rework_events"][0]["duration_sec"], 900);
    assert_eq!(value["rework_events"][1]["kind"], "merge_conflict");
    assert_eq!(value["rework_events"][1]["classification"], "environmental");
    assert!(
        value["rework_events"][1].get("duration_sec").is_none(),
        "an unresolved conflict reports no duration rather than a fabricated 0: {value}"
    );
    let decoded: SweepOutcomeRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

/// Backward compatibility (these fields ship with no `schema_version` bump):
/// a line written before #9444 must still decode, with each field reading as
/// "not observed" rather than failing the whole line — the readers here drop
/// on parse failure, so a breaking decode would erase all historical records.
#[test]
fn a_pre_9444_record_still_decodes_with_no_lineage() {
    let json = r#"{
        "kind": "sweep.outcome",
        "repo": "rjwalters/loom",
        "visibility": "public",
        "issue": 9444,
        "sweep_id": "sweep-issue-9444-0",
        "total_duration_sec": 512,
        "result": "failure",
        "disposition": "env_failure",
        "failure_class": "unclassified:spawn-death"
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::SweepOutcome(r) => {
            assert_eq!(r.attempt_index, None);
            assert_eq!(r.previous_sweep_id, None);
            assert_eq!(r.trigger, None);
            assert_eq!(r.rework_events, None);
        }
        other => panic!("expected SweepOutcome, got {other:?}"),
    }
}
