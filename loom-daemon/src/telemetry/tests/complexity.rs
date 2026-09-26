//! `sweep.outcome` Curator complexity-tier field tests (Issue #8542).
//!
//! A sibling module rather than more lines in `telemetry/tests.rs`, which is
//! at the `scripts/check-file-size-budget.sh` threshold — the same reason
//! `admission_brake.rs` and `role_tick.rs` are split out beside this one.
//!
//! What is asserted here is the wire contract: `complexity` is additive,
//! omitted (never a fabricated `"routine"`) when the issue carried no
//! recognized marker or the fetch was not attempted/failed, and a record
//! emitted by a pre-#8542 daemon must still decode.

use super::super::*;
use super::sweep_outcome;

#[test]
fn sweep_outcome_carries_complexity_when_the_marker_was_read() {
    let record = sweep_outcome();
    let value = serde_json::to_value(&record).unwrap();
    assert_eq!(value["complexity"], "routine");
    let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn sweep_outcome_omits_complexity_when_no_marker_was_read() {
    let base = SweepOutcomeRecord {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Private,
        issue: 8542,
        sweep_id: "sweep-issue-8542-0".to_string(),
        model: None,
        effort: None,
        config: std::collections::BTreeMap::new(),
        phase_durations: Vec::new(),
        total_duration_sec: 10,
        result: SweepResult::Failure,
        pr_number: None,
        tokens_in: None,
        tokens_out: None,
        lines_added: None,
        lines_deleted: None,
        tokens_by_model: None,
        failure_class: None,
        models_used: None,
        doctor_cycles: None,
        judge_verdicts: None,
        runtime: None,
        provider: None,
        profile: None,
        complexity: None,
    };
    let value = serde_json::to_value(&base).unwrap();
    assert!(
        value.get("complexity").is_none(),
        "an unmarked issue must omit complexity, never a fabricated \"routine\": {value}"
    );
    let decoded: SweepOutcomeRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, base);
}

#[test]
fn sweep_outcome_from_a_pre_8542_daemon_still_decodes() {
    // Backward compatibility (no `schema_version` bump accompanies this
    // field): a record emitted by a daemon that predates it must decode,
    // with `complexity` reading as "not observed".
    let json = r#"{
        "kind": "sweep.outcome",
        "repo": "rjwalters/loom",
        "visibility": "public",
        "issue": 8542,
        "sweep_id": "sweep-issue-8542-0",
        "model": "sonnet",
        "total_duration_sec": 512,
        "result": "success",
        "pr_number": 8600
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::SweepOutcome(r) => {
            assert_eq!(r.pr_number, Some(8600));
            assert_eq!(r.complexity, None);
        }
        other => panic!("expected SweepOutcome, got {other:?}"),
    }
}
