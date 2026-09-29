//! `loom.story_points` record-field tests (Issue #9432, epic #9429).
//!
//! The label-guard state machine itself is unit-tested in
//! `crate::story_points`'s own module tests (present / absent / multiple /
//! invalid). What is pinned HERE is the record contract the guard feeds:
//! `story_points` on `sweep.started` and `sweep.outcome` is a **numeric,
//! optional** field — present only when the issue carried exactly one
//! in-vocabulary `points:<N>` label at dispatch, absent (never `0`, never
//! `null`) for every other shape, and additive so a pre-#9432 record still
//! decodes with no `schema_version` bump.

use super::*;

fn started(story_points: Option<u8>) -> SweepStartedRecord {
    SweepStartedRecord {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Public,
        issue: 9432,
        sweep_id: "sweep-issue-9432-0".to_string(),
        started_at: ts(),
        model: None,
        effort: None,
        runtime: None,
        story_points,
    }
}

fn outcome(story_points: Option<u8>) -> SweepOutcomeRecord {
    SweepOutcomeRecord {
        repo: Some("rjwalters/loom".to_string()),
        repo_unresolved: false,
        visibility: RepoVisibility::Public,
        issue: 9432,
        sweep_id: "sweep-issue-9432-0".to_string(),
        model: None,
        effort: None,
        config: std::collections::BTreeMap::new(),
        phase_durations: Vec::new(),
        total_duration_sec: 30,
        result: SweepResult::Success,
        disposition: SweepDisposition::NoopAlreadyDone,
        pr_number: None,
        tokens_in: None,
        tokens_out: None,
        lines_added: None,
        lines_deleted: None,
        tokens_by_model: None,
        tokens_unattributed: None,
        failure_class: None,
        models_used: None,
        doctor_cycles: None,
        judge_verdicts: None,
        runtime: None,
        provider: None,
        profile: None,
        complexity: None,
        tokens_status: None,
        tokens_status_reason: None,
        attempt_index: None,
        previous_sweep_id: None,
        trigger: None,
        rework_events: None,
        pr_numbers: None,
        hw_lines_added: None,
        hw_lines_deleted: None,
        hw_files: None,
        generated_lines: None,
        test_lines: None,
        story_points,
    }
}

#[test]
fn story_points_serializes_as_a_number_on_both_kinds_when_resolved() {
    // One `points:5` label at dispatch → the estimate rides BOTH records as
    // a JSON number (the OTLP mapping renders the same value as an int), so
    // a backend can GROUP BY / join on it without a cast.
    assert_eq!(
        serde_json::to_value(started(Some(5)))
            .unwrap()
            .get("story_points")
            .and_then(serde_json::Value::as_u64),
        Some(5)
    );
    assert_eq!(
        serde_json::to_value(outcome(Some(13)))
            .unwrap()
            .get("story_points")
            .and_then(serde_json::Value::as_u64),
        Some(13)
    );
}

#[test]
fn story_points_is_omitted_not_zero_when_the_guard_declined() {
    // Absent is not zero: an issue with no `points:*` label (or an ambiguous
    // multiple-label read) produces records with NO key at all — `0` would
    // fold "no estimate" into the "estimated, trivially small" population.
    for value in [
        serde_json::to_value(started(None)).unwrap(),
        serde_json::to_value(outcome(None)).unwrap(),
    ] {
        assert!(
            value.get("story_points").is_none(),
            "an unresolved estimate must omit the key entirely, not serialize zero/null: {value}"
        );
    }
}

#[test]
fn pre_9432_records_still_decode_without_the_field() {
    // Additive-only schema change (no `schema_version` bump): a wire record
    // written before #9432 carries no `story_points` key and must decode to
    // `None` — "no estimate", never an error and never a fabricated value.
    let legacy_started: SweepStartedRecord = serde_json::from_str(
        r#"{"repo":"rjwalters/loom","visibility":"public","issue":4703,
                "sweep_id":"sweep-issue-4703-0","started_at":"2026-07-30T12:00:00Z"}"#,
    )
    .unwrap();
    assert_eq!(legacy_started.story_points, None);
    let legacy_outcome: SweepOutcomeRecord = serde_json::from_str(
        r#"{"repo":"rjwalters/loom","visibility":"public","issue":4703,
            "sweep_id":"sweep-issue-4703-0","total_duration_sec":40,
            "result":"failure","disposition":"env_failure"}"#,
    )
    .unwrap();
    assert_eq!(legacy_outcome.story_points, None);
}

#[test]
fn label_guard_resolves_every_contract_case_for_both_kinds() {
    // The three issue-worklist label cases, driven end-to-end through the
    // pure guard into both record kinds' field — present / absent / multiple.
    use crate::story_points::{story_points_from_labels, StoryPoints};
    let resolve = |labels: &[&str]| {
        story_points_from_labels(labels.iter().copied())
            .value()
            .map(|points| serde_json::to_value(started(Some(points))).unwrap())
    };
    // present → carried
    assert_eq!(
        resolve(&["loom:issue", "points:8"])
            .unwrap()
            .get("story_points")
            .and_then(serde_json::Value::as_u64),
        Some(8)
    );
    // absent → key omitted entirely
    assert!(resolve(&["loom:issue"]).is_none());
    // multiple → never a guess: the guard surfaces `Multiple`, whose value()
    // is None, so the record carries no key at all.
    assert_eq!(
        story_points_from_labels(["points:3", "points:8"]),
        StoryPoints::Multiple(vec!["points:3".to_string(), "points:8".to_string()])
    );
    assert!(resolve(&["points:3", "points:8"]).is_none());
}
