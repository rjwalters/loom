//! Fleet-wide quarantine-stash visibility (#5692): pins the
//! `per_repo[].stash` `--json` contract and the compact human-age
//! formatter that renders alongside it.
use super::{build_status_json_value, format_stash_age};
use crate::cli::status::sample_report::sample_report;

fn no_update() -> loom_daemon::self_update::SelfUpdateStatus {
    loom_daemon::self_update::SelfUpdateStatus {
        built_commit: "abc1234".to_string(),
        source_commit: None,
        update_available: None,
        commits_behind: None,
        hours_behind: None,
    }
}

fn repo_with_stashes(
    total: usize,
    quarantine: usize,
    oldest_age_secs: Option<u64>,
) -> loom_daemon::types::RepoStatus {
    repo_with_stashes_full(total, quarantine, oldest_age_secs, 0, None)
}

/// #5512: the same fixture as [`repo_with_stashes`], with the two
/// non-quarantine-unrecoverable fields also settable.
fn repo_with_stashes_full(
    total: usize,
    quarantine: usize,
    oldest_age_secs: Option<u64>,
    non_quarantine_unrecoverable_count: usize,
    non_quarantine_unrecoverable_oldest_age_secs: Option<u64>,
) -> loom_daemon::types::RepoStatus {
    loom_daemon::types::RepoStatus {
        root: std::path::PathBuf::from("/repos/loom"),
        maintain_only: None,
        weight: 100,
        in_flight_count: 0,
        health_gate_halted: false,
        quarantined_issues: vec![],
        health_gate_not_evaluated: false,
        health_gate_not_evaluated_reason: None,
        health_gate_enabled: None,
        health_gate_verdict_at: None,
        root_missing: false,
        health_gate_deferred: false,
        health_gate_deferred_reason: None,
        health_gate_verdict_tier: None,
        role_runner_enabled: false,
        role_runner_roles: vec![],
        role_runner_intervals: std::collections::BTreeMap::new(),
        role_runner_on_idle_roles: vec![],
        role_runner_on_idle_promotions: vec![],
        role_runner_env_override: None,
        role_runner_shard: None,
        token_pool_dir: None,
        ranking_present: false,
        ranking_age_secs: None,
        stash_total_count: total,
        stash_quarantine_count: quarantine,
        stash_oldest_age_secs: oldest_age_secs,
        stash_non_quarantine_unrecoverable_count: non_quarantine_unrecoverable_count,
        stash_non_quarantine_unrecoverable_oldest_age_secs:
            non_quarantine_unrecoverable_oldest_age_secs,
        sweep_command_missing: false,
    }
}

#[test]
fn per_repo_json_carries_stash_counts_and_oldest_age() {
    let mut report = sample_report();
    report.per_repo = vec![repo_with_stashes(5, 2, Some(3600))];
    let value = build_status_json_value(&report, None, &no_update(), None, None, None);
    let stash = &value["per_repo"][0]["stash"];
    assert_eq!(stash["total_count"], 5);
    assert_eq!(stash["quarantine_count"], 2);
    assert_eq!(stash["oldest_age_secs"], 3600);
}

#[test]
fn per_repo_json_stash_oldest_age_is_null_with_zero_stashes() {
    let mut report = sample_report();
    report.per_repo = vec![repo_with_stashes(0, 0, None)];
    let value = build_status_json_value(&report, None, &no_update(), None, None, None);
    let stash = &value["per_repo"][0]["stash"];
    assert_eq!(stash["total_count"], 0);
    assert_eq!(stash["quarantine_count"], 0);
    assert!(stash["oldest_age_secs"].is_null());
}

#[test]
fn per_repo_json_carries_non_quarantine_unrecoverable_count_and_age() {
    let mut report = sample_report();
    report.per_repo = vec![repo_with_stashes_full(5, 2, Some(3600), 2, Some(7200))];
    let value = build_status_json_value(&report, None, &no_update(), None, None, None);
    let stash = &value["per_repo"][0]["stash"];
    assert_eq!(stash["non_quarantine_unrecoverable_count"], 2);
    assert_eq!(stash["non_quarantine_unrecoverable_oldest_age_secs"], 7200);
}

#[test]
fn per_repo_json_non_quarantine_unrecoverable_oldest_age_is_null_when_count_is_zero() {
    let mut report = sample_report();
    report.per_repo = vec![repo_with_stashes(5, 5, Some(3600))];
    let value = build_status_json_value(&report, None, &no_update(), None, None, None);
    let stash = &value["per_repo"][0]["stash"];
    assert_eq!(stash["non_quarantine_unrecoverable_count"], 0);
    assert!(stash["non_quarantine_unrecoverable_oldest_age_secs"].is_null());
}

#[test]
fn format_stash_age_prefers_the_largest_whole_unit() {
    assert_eq!(format_stash_age(30), "30s");
    assert_eq!(format_stash_age(90), "1m");
    assert_eq!(format_stash_age(3600), "1h");
    assert_eq!(format_stash_age(90_000), "1d");
    // #5690's fleet audit: "12 days" of accumulation is the motivating
    // scale for this surface.
    assert_eq!(format_stash_age(12 * 86_400), "12d");
}
