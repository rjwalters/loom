//! Issue #9005: `tokens.snapshot`'s weekly (7-day) axis, read from the
//! `.ranking.weekly.json` sidecar beside `.ranking`.

use super::*;
use crate::tokens_pool::check::{format_ranking_lines, AccountResult, ProbeReport};
use crate::tokens_pool::ranking_weekly::{write_weekly_utilization_sidecar, SIDECAR_FILE_NAME};

fn pool_dir(workspace: &Path) -> std::path::PathBuf {
    let dir = workspace.join(".loom/tokens");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn weekly_utilization_joins_the_ranking_rows_by_account_name() {
    // The real writers end to end: `.ranking` from the probe report, then
    // the sidecar from the same report, as `tokens check --ranking` does.
    let mut a = AccountResult::new("agent-1", "available");
    a.s5h_utilization = Some(0.42);
    a.s7d_utilization = Some(0.63);
    let mut b = AccountResult::new("agent-2", "exhausted");
    b.s5h_utilization = Some(0.10);
    b.s7d_utilization = Some(1.0);
    // A probe with a 5h reading but no 7d one: weekly stays unknown.
    let mut c = AccountResult::new("agent-3", "available");
    c.s5h_utilization = Some(0.05);
    let report = ProbeReport {
        ranked_at: "2026-09-27T00:00:00Z".into(),
        accounts: vec![a, b, c],
    };

    let workspace = tempfile::tempdir().unwrap();
    let dir = pool_dir(workspace.path());
    std::fs::write(dir.join(".ranking"), format_ranking_lines(&report)).unwrap();
    write_weekly_utilization_sidecar(&report, &dir, Utc::now()).unwrap();

    let record = sample_token_snapshot(workspace.path());
    assert_eq!(record.accounts.len(), 3);
    assert_eq!(record.accounts[0].usage_fraction, Some(0.42));
    assert_eq!(record.accounts[0].usage_fraction_weekly, Some(0.63));
    assert_eq!(record.accounts[1].usage_fraction_weekly, Some(1.0));
    assert_eq!(
        record.accounts[2].usage_fraction_weekly, None,
        "a missing 7d reading is unknown, not a fabricated 0"
    );
}

#[test]
fn missing_sidecar_leaves_weekly_absent_and_5h_untouched() {
    let workspace = tempfile::tempdir().unwrap();
    let dir = pool_dir(workspace.path());
    std::fs::write(dir.join(".ranking"), "agent-1|available|0.42\n").unwrap();
    let record = sample_token_snapshot(workspace.path());
    assert_eq!(record.accounts[0].usage_fraction, Some(0.42));
    assert_eq!(record.accounts[0].usage_fraction_weekly, None);
}

#[test]
fn unparseable_sidecar_degrades_to_absent() {
    let workspace = tempfile::tempdir().unwrap();
    let dir = pool_dir(workspace.path());
    std::fs::write(dir.join(".ranking"), "agent-1|available|0.42\n").unwrap();
    std::fs::write(dir.join(SIDECAR_FILE_NAME), "{\"schema\":1,\"accounts\":").unwrap();
    let record = sample_token_snapshot(workspace.path());
    assert_eq!(record.accounts[0].usage_fraction_weekly, None);
}
