//! `role_tick.outcome` schema tests (Issue #8056) — the seventh record kind
//! and the `CURRENT_SCHEMA_VERSION` bump it forces.
//!
//! A sibling module rather than more lines in `telemetry/tests.rs`, which is
//! at the `scripts/check-file-size-budget.sh` threshold.

use super::super::*;
use super::ts;

/// The fully-populated fixture, shared with the parent module's
/// every-record round-trip sweeps.
pub(super) fn role_tick_outcome() -> TelemetryRecord {
    TelemetryRecord::RoleTickOutcome(RoleTickOutcomeRecord {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Public,
        role: "judge".to_string(),
        started_at: ts(),
        duration_sec: 137,
        result: RoleTickResult::Success,
        model: Some("claude-sonnet-5".to_string()),
        effort: Some("high".to_string()),
        detail: None,
        tokens_by_model: Some(vec![ModelUsageTotals {
            model: "claude-sonnet-5".to_string(),
            speed: "standard".to_string(),
            service_tier: "standard".to_string(),
            input: 1_200,
            cache_read: 40_000,
            cache_write_5m: 900,
            cache_write_1h: 0,
            output: 3_400,
        }]),
        models_used: Some(vec!["claude-sonnet-5".to_string()]),
        actions: Some(RoleTickActions {
            issues_labeled: 3,
            prs_merged: 1,
            comments_posted: 4,
        }),
    })
}

// ------------------------------------------------------------------
// role_tick.outcome (Issue #8056) — the seventh record kind, and the
// reason CURRENT_SCHEMA_VERSION moved to 2.
// ------------------------------------------------------------------

#[test]
fn role_tick_outcome_serializes_under_its_kind_tag() {
    let value = serde_json::to_value(role_tick_outcome()).unwrap();
    assert_eq!(value.get("kind").and_then(serde_json::Value::as_str), Some("role_tick.outcome"),);
    // The tag flattens into the same object as the payload fields, so a
    // backend pattern-matches one flat object per record.
    assert_eq!(value.get("role").and_then(serde_json::Value::as_str), Some("judge"));
    assert_eq!(
        value
            .get("duration_sec")
            .and_then(serde_json::Value::as_i64),
        Some(137)
    );
    assert_eq!(value.get("result").and_then(serde_json::Value::as_str), Some("success"));
}

#[test]
fn role_tick_result_serializes_every_skip_class_distinctly() {
    let wire = |r: RoleTickResult| serde_json::to_value(r).unwrap();
    assert_eq!(wire(RoleTickResult::Success), "success");
    assert_eq!(wire(RoleTickResult::Failure), "failure");
    assert_eq!(wire(RoleTickResult::RuntimeRejected), "runtime_rejected");
    assert_eq!(wire(RoleTickResult::SkippedNoTokenPool), "skipped_no_token_pool");
    assert_eq!(wire(RoleTickResult::SkippedPoolExhausted), "skipped_pool_exhausted");
    assert_eq!(
        wire(RoleTickResult::SkippedModelRuntimeMismatch),
        "skipped_model_runtime_mismatch"
    );
    assert_eq!(wire(RoleTickResult::SkippedLoad), "skipped_load");
}

/// The "unknown != zero" contract, on the wire: an unobserved measurement is
/// an ABSENT key, never a `0`/`[]`/`null`. #8057's summary depends on telling
/// "not observed" from "observed, none".
#[test]
fn role_tick_outcome_omits_every_unobserved_field() {
    let record = RoleTickOutcomeRecord {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Private,
        role: "champion".to_string(),
        started_at: ts(),
        duration_sec: 0,
        result: RoleTickResult::SkippedNoTokenPool,
        model: None,
        effort: None,
        detail: Some("no-token-pool".to_string()),
        tokens_by_model: None,
        models_used: None,
        actions: None,
    };
    let value = serde_json::to_value(&record).unwrap();
    for key in [
        "model",
        "effort",
        "tokens_by_model",
        "models_used",
        "actions",
    ] {
        assert!(
            value.get(key).is_none(),
            "{key} must be omitted, not serialized as a zero/empty value"
        );
    }
    // The fields that ARE decided (not measured) stay mandatory.
    assert_eq!(
        value
            .get("duration_sec")
            .and_then(serde_json::Value::as_i64),
        Some(0)
    );
    assert_eq!(value.get("detail").and_then(serde_json::Value::as_str), Some("no-token-pool"));
}

#[test]
fn role_tick_outcome_keeps_an_observed_zero_action_count() {
    let mut record = match role_tick_outcome() {
        TelemetryRecord::RoleTickOutcome(r) => r,
        other => panic!("fixture changed: {other:?}"),
    };
    record.actions = Some(RoleTickActions::default());
    let value = serde_json::to_value(&record).unwrap();
    assert_eq!(value["actions"]["issues_labeled"], 0);
    assert_eq!(value["actions"]["prs_merged"], 0);
    assert_eq!(value["actions"]["comments_posted"], 0);
    let decoded: RoleTickOutcomeRecord = serde_json::from_value(value).unwrap();
    assert_eq!(decoded.actions, Some(RoleTickActions::default()));
}

/// A `role_tick.outcome` line written by a daemon that predates any of the
/// optional fields (only the mandatory ones present) must still decode — the
/// same forward-compatibility contract every other record kind holds.
#[test]
fn role_tick_outcome_decodes_a_minimal_line() {
    let line = r#"{"kind":"role_tick.outcome","repo":"rjwalters/loom","role":"guide",
        "started_at":"2026-07-30T12:00:00Z","duration_sec":12,"result":"failure"}"#;
    let record: TelemetryRecord = serde_json::from_str(line).unwrap();
    let TelemetryRecord::RoleTickOutcome(r) = record else {
        panic!("wrong kind decoded");
    };
    assert_eq!(r.role, "guide");
    assert_eq!(r.result, RoleTickResult::Failure);
    // No `visibility` on the wire ⇒ Private, never Public.
    assert_eq!(r.visibility, RepoVisibility::Private);
    assert_eq!(r.model, None);
    assert_eq!(r.actions, None);
}

#[test]
fn role_tick_result_spawned_separates_pre_spawn_skips() {
    for result in [
        RoleTickResult::Success,
        RoleTickResult::Failure,
        RoleTickResult::SkippedLoad,
    ] {
        assert!(result.spawned(), "{result:?} launched a session");
    }
    for result in [
        RoleTickResult::RuntimeRejected,
        RoleTickResult::SkippedNoTokenPool,
        RoleTickResult::SkippedPoolExhausted,
        RoleTickResult::SkippedModelRuntimeMismatch,
    ] {
        assert!(
            !result.spawned(),
            "{result:?} bailed out before any spawn, so an absent token count is \
             'correctly nothing', not 'unknown'"
        );
    }
}

// ------------------------------------------------------------------
// schema_version = 2 (Issue #8056) and pre-bump compatibility.
// ------------------------------------------------------------------

/// The bump is load-bearing: `role_tick.outcome` is a NEW record kind, which
/// is precisely what a mixed-version backend must gate on (an additive
/// optional field is not).
#[test]
fn current_schema_version_is_two_for_the_new_record_kind() {
    assert_eq!(CURRENT_SCHEMA_VERSION, 2);
}

/// A journal line written by a pre-bump (`schema_version: 1`) daemon must
/// still parse here, unchanged — mirrors
/// `sweep_outcomes::tests::read_all_defaults_crash_classification_for_pre_existing_lines`.
/// The version is data the backend gates on, never a parse gate.
#[test]
fn a_pre_bump_schema_version_1_envelope_still_parses() {
    let line = r#"{"schema_version":1,"emitted_at":"2026-07-30T12:00:00Z","host_id":"host-abc",
        "record":{"kind":"sweep.outcome","repo":"rjwalters/loom","visibility":"public","issue":4703,
        "sweep_id":"sweep-issue-4703-0","total_duration_sec":900,"result":"success"}}"#;
    let envelope: TelemetryEnvelope = serde_json::from_str(line).unwrap();
    assert_eq!(
        envelope.schema_version, 1,
        "the wire value is preserved verbatim, never rewritten to CURRENT"
    );
    let TelemetryRecord::SweepOutcome(record) = envelope.record else {
        panic!("wrong kind decoded");
    };
    assert_eq!(record.issue, 4703);
    assert_eq!(record.result, SweepResult::Success);
    // Every #8056 Phase-1 field is absent on a pre-bump line and decodes to
    // "not observed", never to a fabricated zero.
    assert_eq!(record.failure_class, None);
    assert_eq!(record.models_used, None);
    assert_eq!(record.doctor_cycles, None);
}
