//! `daemon.event` record-kind tests (Issue #8760, G4 of #8714): envelope
//! round-trip and per-kind schema versioning, for one fixture per named
//! topic family (`daemon.drain.*`, `daemon.capacity.advisory`,
//! `daemon.preflight.advisory`, `epic.issue.*`).

use super::*;

fn daemon_event(topic: &str, payload: serde_json::Value) -> TelemetryRecord {
    TelemetryRecord::DaemonEvent(DaemonEventRecord {
        topic: topic.to_string(),
        payload,
    })
}

#[test]
fn every_named_topic_family_round_trips() {
    let fixtures = [
        daemon_event(
            "daemon.drain.started",
            serde_json::json!({"in_flight": 2, "timeout_secs": 300, "force_after_timeout": true}),
        ),
        daemon_event(
            "daemon.capacity.advisory",
            serde_json::json!({
                "pressured": true, "queued": 4, "healthy_accounts": 1,
                "exhausted_accounts": 2, "total_accounts": 3, "message": "add accounts",
            }),
        ),
        daemon_event(
            "daemon.preflight.advisory",
            serde_json::json!({
                "workspace_root": "/repos/loom", "consecutive_deaths": 4,
                "marker": "preflight-mcp-failed", "message": "check .mcp.json",
            }),
        ),
        daemon_event(
            "epic.issue.123.decompose",
            serde_json::json!({"epic": 123, "state": "epic:needs_decomp"}),
        ),
    ];
    for record in fixtures {
        let envelope = TelemetryEnvelope::new("host-abc", record.clone());
        let json = serde_json::to_string(&envelope).unwrap();
        let decoded: TelemetryEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, decoded, "round-trip mismatch for {record:?}");
        assert!(json.contains("\"kind\":\"daemon.event\""));

        let bare = serde_json::to_string(&record).unwrap();
        let decoded_record: TelemetryRecord = serde_json::from_str(&bare).unwrap();
        assert_eq!(record, decoded_record);
    }
}

#[test]
fn daemon_event_envelopes_carry_their_own_schema_version() {
    // The `trace.span` (3) / `sweep.identity` (4) / `session.summary` (5) /
    // `session.analysis` (6) per-kind pattern: only daemon.event envelopes
    // carry 7.
    let envelope = TelemetryEnvelope::new(
        "host-abc",
        daemon_event("daemon.drain.started", serde_json::json!({"in_flight": 0})),
    );
    assert_eq!(envelope.schema_version, 7);
    for (record, version) in [
        (sweep_started(), u64::from(CURRENT_SCHEMA_VERSION)),
        (role_tick_outcome(), u64::from(CURRENT_SCHEMA_VERSION)),
    ] {
        assert_eq!(u64::from(TelemetryEnvelope::new("host-abc", record).schema_version), version);
    }
}

#[test]
fn the_topic_field_is_the_exact_bus_topic_string() {
    let record = daemon_event("epic.issue.456.close", serde_json::json!({"epic": 456}));
    let TelemetryRecord::DaemonEvent(inner) = &record else {
        panic!("fixture");
    };
    assert_eq!(inner.topic, "epic.issue.456.close");
}
