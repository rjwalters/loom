//! `session.analysis` record-kind tests (Issue #8760, G3 part 2 of #8714):
//! envelope round-trip, per-kind schema versioning, and the wire-safety
//! field-presence contract (unknown optionals stay absent).

use super::*;

pub fn session_analysis() -> TelemetryRecord {
    TelemetryRecord::SessionAnalysis(SessionAnalysisRecord {
        repo: "loom".to_string(),
        visibility: RepoVisibility::Private,
        session_id: "uuid-a".to_string(),
        parent_session_id: Some("uuid-parent".to_string()),
        retry_loops: vec![RetryLoop {
            tool: "Bash".to_string(),
            length: 3,
        }],
        longest_tool_call: Some(LongestToolCall {
            tool: "Bash".to_string(),
            duration_ms: 5_000,
        }),
        cost_usd: Some(0.042),
        anomalies: vec![AnomalyFlag::HighTokenUsage],
    })
}

#[test]
fn session_analysis_round_trips() {
    let envelope = TelemetryEnvelope::new("host-abc", session_analysis());
    let json = serde_json::to_string(&envelope).unwrap();
    let decoded: TelemetryEnvelope = serde_json::from_str(&json).unwrap();
    assert_eq!(envelope, decoded);
    assert!(json.contains("\"kind\":\"session.analysis\""));
    assert!(json.contains("\"parent_session_id\":\"uuid-parent\""));
    // Bare record round-trip too (consumed directly outside the envelope,
    // same contract as every other kind).
    let record = session_analysis();
    let bare = serde_json::to_string(&record).unwrap();
    let decoded_record: TelemetryRecord = serde_json::from_str(&bare).unwrap();
    assert_eq!(record, decoded_record);
}

#[test]
fn session_analysis_envelopes_carry_their_own_schema_version() {
    // The `trace.span` (3) / `sweep.identity` (4) / `session.summary` (5)
    // per-kind pattern: only session-analysis envelopes carry 6, so every
    // pre-existing kind's version is unchanged for a mixed-version fleet's
    // backend.
    let envelope = TelemetryEnvelope::new("host-abc", session_analysis());
    assert_eq!(envelope.schema_version, 6);
    for (record, version) in [
        (sweep_started(), u64::from(CURRENT_SCHEMA_VERSION)),
        (role_tick_outcome(), u64::from(CURRENT_SCHEMA_VERSION)),
    ] {
        assert_eq!(u64::from(TelemetryEnvelope::new("host-abc", record).schema_version), version);
    }
}

#[test]
fn session_analysis_unknown_optionals_stay_absent_never_fabricated() {
    let mut record = session_analysis();
    let TelemetryRecord::SessionAnalysis(inner) = &mut record else {
        panic!("fixture");
    };
    inner.parent_session_id = None;
    inner.longest_tool_call = None;
    inner.cost_usd = None;
    inner.retry_loops = Vec::new();
    inner.anomalies = Vec::new();
    let json = serde_json::to_string(&record).unwrap();
    for absent in ["parent_session_id", "longest_tool_call", "cost_usd"] {
        assert!(!json.contains(&format!("\"{absent}\"")), "{absent} fabricated: {json}");
    }
    // `retry_loops`/`anomalies` are present-but-empty, not omitted — the
    // "computed and found none" contract, distinguishable from "not
    // computed" — so their keys DO appear, as empty arrays.
    assert!(json.contains("\"retry_loops\":[]"));
    assert!(json.contains("\"anomalies\":[]"));
    let decoded: TelemetryRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, record);
}
