//! `session.summary` record-kind tests (Issue #8757, G3 of #8714):
//! envelope round-trip, per-kind schema versioning, and the wire-safety
//! field-presence contract (unknown optionals stay absent).

use super::*;

pub fn session_summary() -> TelemetryRecord {
    TelemetryRecord::SessionSummary(SessionSummaryRecord {
        repo: Some("rjwalters/loom".to_string()),
        visibility: RepoVisibility::Private,
        session_id: "uuid-a".to_string(),
        parent_session_id: Some("uuid-parent".to_string()),
        runtime: "claude".to_string(),
        role: Some("builder".to_string()),
        issue: Some(8757),
        pr_number: Some(9460),
        session_kind: Some(SessionKind::Sweep),
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
    inner.pr_number = None;
    inner.outcome = None;
    // #9445: an unresolvable repo is an ABSENT slug, never a directory name.
    inner.repo = None;
    let json = serde_json::to_string(&record).unwrap();
    for absent in [
        "parent_session_id",
        "role",
        "issue",
        "pr_number",
        "outcome",
        "repo",
    ] {
        assert!(!json.contains(&format!("\"{absent}\"")), "{absent} fabricated: {json}");
    }
    // Measured fields are always present, even at zero.
    assert!(json.contains("\"turns\":0") || json.contains("\"turns\":1"));
}

/// #9445: the gateway's log allowlist must keep `loom.session_kind` — an
/// attribute the collector drops is one no query can read, so this is as
/// load-bearing as emitting it (the `collector_keeps_every_eta_log_attribute`
/// precedent).
#[test]
fn collector_keeps_the_session_summary_join_keys() {
    const CONFIG: &str = include_str!("../../../../defaults/observability/collector/config.yaml");
    let log_keep = CONFIG
        .lines()
        .find(|l| {
            l.contains("keep_keys(attributes, [\"loom.repo\"") && l.contains("loom.ci.chunk_index")
        })
        .expect("the transform/privacy log keep_keys line");
    for key in [
        "loom.repo",
        "loom.issue",
        "loom.pr_number",
        "loom.session_kind",
    ] {
        assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
    }
}

/// #9445: the join keys survive the wire, and `session_kind` decodes back to
/// the same variant (a record predating the field decodes as absent, not as a
/// guessed `interactive`).
#[test]
fn session_summary_carries_its_join_keys_over_the_wire() {
    let record = session_summary();
    let json = serde_json::to_string(&record).unwrap();
    assert!(json.contains("\"repo\":\"rjwalters/loom\""), "{json}");
    assert!(json.contains("\"issue\":8757"), "{json}");
    assert!(json.contains("\"pr_number\":9460"), "{json}");
    assert!(json.contains("\"session_kind\":\"sweep\""), "{json}");
    let decoded: TelemetryRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(record, decoded);

    let TelemetryRecord::SessionSummary(legacy) =
        serde_json::from_str::<TelemetryRecord>(&json.replace("\"session_kind\":\"sweep\",", ""))
            .unwrap()
    else {
        panic!("kind tag");
    };
    assert_eq!(legacy.session_kind, None, "a pre-#9445 record stays honestly unlabelled");
}
