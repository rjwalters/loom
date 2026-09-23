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
                limit_window_reset_at: Some(ts()),
                exhausted: false,
            },
            TokenAccountState {
                account: "agent-2".to_string(),
                provider: "codex".to_string(),
                rank: None,
                usage_fraction: None,
                limit_window_reset_at: None,
                exhausted: true,
            },
        ],
    })
}
