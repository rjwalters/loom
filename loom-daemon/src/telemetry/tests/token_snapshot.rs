use super::*;

pub(super) fn tokens_snapshot() -> TelemetryRecord {
    TelemetryRecord::TokensSnapshot(TokenSnapshotRecord {
        captured_at: ts(),
        accounts: vec![
            TokenAccountState {
                account: "agent-1".to_string(),
                provider: "claude".to_string(),
                rank: Some(0),
                usage_fraction: Some(0.42),
                usage_fraction_weekly: Some(0.37),
                limit_window_reset_at: Some(ts()),
                exhausted: false,
            },
            TokenAccountState {
                account: "agent-2".to_string(),
                provider: "codex".to_string(),
                rank: None,
                usage_fraction: None,
                usage_fraction_weekly: None,
                limit_window_reset_at: None,
                exhausted: true,
            },
        ],
    })
}

/// Issue #9005: `usage_fraction_weekly` is additive — a record from a daemon
/// that predates it still decodes (as unknown), and an unknown value is
/// omitted from the wire rather than written as `null` or `0`.
#[test]
fn usage_fraction_weekly_is_optional_on_the_wire() {
    let legacy =
        r#"{"account":"agent-1","provider":"claude","usage_fraction":0.4,"exhausted":false}"#;
    let decoded: TokenAccountState = serde_json::from_str(legacy).unwrap();
    assert_eq!(decoded.usage_fraction_weekly, None);
    let json = serde_json::to_value(&decoded).unwrap();
    assert!(json.get("usage_fraction_weekly").is_none());

    let TelemetryRecord::TokensSnapshot(snapshot) = tokens_snapshot() else {
        unreachable!("tokens_snapshot() builds a tokens.snapshot record");
    };
    let json = serde_json::to_value(&snapshot.accounts[0]).unwrap();
    assert_eq!(json["usage_fraction_weekly"], serde_json::json!(0.37));
}
