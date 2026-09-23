//! `session.summary` record-kind tests (Issue #8757, G3 of #8714):
//! envelope round-trip, per-kind schema versioning, and the wire-safety
//! field-presence contract (unknown optionals stay absent).

use super::*;

pub fn session_summary() -> TelemetryRecord {
    TelemetryRecord::SessionSummary(SessionSummaryRecord {
        repo: "loom".to_string(),
        visibility: RepoVisibility::Private,
        session_id: "uuid-a".to_string(),
        parent_session_id: Some("uuid-parent".to_string()),
        runtime: "claude".to_string(),
        role: Some("builder".to_string()),
        issue: Some(8757),
        pr_number: None,
        models: vec!["claude-sonnet-5".to_string()],
        tokens_input: 12,
        tokens_output: 24,
        tokens_cache_read: 100,
        tokens_cache_write: 10,
        wall_ms: 181_000,
        turns: 1,
        tool_calls: vec![ToolCallCount {
            tool: "Bash".to_string(),
            count: 2,
        }],
        tool_errors: 1,
        outcome: None,
    })
}

#[test]
fn session_summary_round_trips() {
    let envelope = TelemetryEnvelope::new("host-abc", session_summary());
    let json = serde_json::to_string(&envelope).unwrap();
    let decoded: TelemetryEnvelope = serde_json::from_str(&json).unwrap();
    assert_eq!(envelope, decoded);
    // The kind tag and the parent linkage survive the wire.
    assert!(json.contains("\"kind\":\"session.summary\""));
    assert!(json.contains("\"parent_session_id\":\"uuid-parent\""));
    // Bare record round-trip too (consumed directly outside the envelope,
    // same contract as every other kind — see tests.rs's
    // bare_record_round_trips_for_every_record_kind).
    let record = session_summary();
    let bare = serde_json::to_string(&record).unwrap();
    let decoded_record: TelemetryRecord = serde_json::from_str(&bare).unwrap();
    assert_eq!(record, decoded_record);
}

#[test]
fn session_summary_envelopes_carry_their_own_schema_version() {
    // The `trace.span` (3) / `sweep.identity` (4) per-kind pattern: only
    // session-summary envelopes carry 5, so every pre-existing kind's
    // version is unchanged for a mixed-version fleet's backend.
    let envelope = TelemetryEnvelope::new("host-abc", session_summary());
    assert_eq!(envelope.schema_version, 5);
    // And every other kind still stamps CURRENT_SCHEMA_VERSION (or its own
    // pre-established per-kind version).
    for (record, version) in [
        (sweep_started(), u64::from(CURRENT_SCHEMA_VERSION)),
        (role_tick_outcome(), u64::from(CURRENT_SCHEMA_VERSION)),
    ] {
        assert_eq!(u64::from(TelemetryEnvelope::new("host-abc", record).schema_version), version);
    }
}

#[test]
fn session_summary_unknown_optionals_stay_absent_never_fabricated() {
    let mut record = session_summary();
    let TelemetryRecord::SessionSummary(inner) = &mut record else {
        panic!("fixture");
    };
    inner.parent_session_id = None;
    inner.role = None;
    inner.issue = None;
    inner.outcome = None;
    let json = serde_json::to_string(&record).unwrap();
    for absent in ["parent_session_id", "role", "issue", "pr_number", "outcome"] {
        assert!(!json.contains(&format!("\"{absent}\"")), "{absent} fabricated: {json}");
    }
    // Measured fields are always present, even at zero.
    assert!(json.contains("\"turns\":0") || json.contains("\"turns\":1"));
}
