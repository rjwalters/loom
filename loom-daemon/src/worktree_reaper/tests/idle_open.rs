//! Issue #12458: the reaper's treatment of an **open** issue / PR whose
//! worktree has sat idle — reapable once pushed and clean, kept otherwise.
//!
//! A sibling of `worktree_reaper.rs`'s own `mod tests` (whose fixtures it
//! inherits through `use super::*`) because that file is at its file-size
//! ratchet baseline.

use super::*;

// ===================================================================
// Idle open items (#12458)
// ===================================================================

fn idle_open_opts() -> CleanOptions {
    CleanOptions {
        idle_open_secs: Some(DEFAULT_IDLE_OPEN_SECS),
        ..default_opts()
    }
}

fn open_issue(idle: bool) -> ProbeSpec {
    ProbeSpec {
        issue_state: "OPEN".to_string(),
        pr_status: PrStatus::Open,
        idle,
        ..ProbeSpec::default()
    }
}

#[test]
#[serial]
fn test_idle_open_issue_worktree_is_reaped() {
    let repo = make_repo(&[(700, true)]);
    let (report, removed) = run_pass(repo.path(), &open_issue(true), &idle_open_opts());
    assert_eq!(removed, vec![700]);
    assert_eq!(report.removed, vec![700]);
}

#[test]
#[serial]
fn test_idle_open_issue_is_kept_when_the_period_is_not_configured() {
    let repo = make_repo(&[(701, true)]);
    let (_, removed) = run_pass(repo.path(), &open_issue(true), &default_opts());
    assert!(removed.is_empty(), "no idleOpenSecs means open items are never reaped");
}

#[test]
#[serial]
fn test_open_issue_worktree_with_recent_activity_is_kept() {
    let repo = make_repo(&[(702, true)]);
    let (report, removed) = run_pass(repo.path(), &open_issue(false), &idle_open_opts());
    assert!(removed.is_empty());
    assert_eq!(report.skipped[0].0, 702);
}

#[test]
#[serial]
fn test_idle_open_issue_with_unsaved_work_is_kept() {
    let repo = make_repo(&[(703, true)]);
    let spec = ProbeSpec {
        unsaved: true,
        ..open_issue(true)
    };
    let (report, removed) = run_pass(repo.path(), &spec, &idle_open_opts());
    assert!(removed.is_empty());
    assert_eq!(report.skipped[0].0, 703);
}

#[test]
#[serial]
fn test_idle_open_issue_with_unpushed_commits_is_kept() {
    let repo = make_repo(&[(704, true)]);
    let spec = ProbeSpec {
        branch_reachable: false,
        ..open_issue(true)
    };
    let (report, removed) = run_pass(repo.path(), &spec, &idle_open_opts());
    assert!(removed.is_empty());
    assert!(report.skipped[0].1.contains("not fully pushed"), "{:?}", report.skipped);
}

#[test]
#[serial]
fn test_idle_open_issue_held_by_a_live_session_is_kept() {
    let repo = make_repo(&[(705, true)]);
    let mut spec = open_issue(true);
    spec.active.insert(705);
    let (_, removed) = run_pass(repo.path(), &spec, &idle_open_opts());
    assert!(removed.is_empty(), "a live spawn-loop claim holds the worktree");

    let repo = make_repo(&[(706, true)]);
    let spec = ProbeSpec {
        in_use: true,
        ..open_issue(true)
    };
    let (_, removed) = run_pass(repo.path(), &spec, &idle_open_opts());
    assert!(removed.is_empty(), "a .loom-in-use marker holds the worktree");
}

#[test]
#[serial]
fn test_unknown_issue_state_is_never_treated_as_idle_open() {
    let repo = make_repo(&[(707, true)]);
    let spec = ProbeSpec {
        issue_state: "UNKNOWN".to_string(),
        ..open_issue(true)
    };
    let (_, removed) = run_pass(repo.path(), &spec, &idle_open_opts());
    assert!(removed.is_empty());
}

#[test]
#[serial]
fn test_idle_open_pr_worktree_is_reaped_and_dirty_or_unpushed_is_kept() {
    let repo = make_pr_repo(&[(710, true)]);
    let spec = ProbeSpec {
        pr_status: PrStatus::Open,
        idle: true,
        ..ProbeSpec::default()
    };
    let (_, removed) = run_pr_pass(repo.path(), &spec, &idle_open_opts());
    assert_eq!(removed, vec![710]);

    for spec in [
        ProbeSpec {
            uncommitted: true,
            ..spec.clone()
        },
        ProbeSpec {
            branch_reachable: false,
            ..spec.clone()
        },
        ProbeSpec {
            idle: false,
            ..spec.clone()
        },
    ] {
        let repo = make_pr_repo(&[(711, true)]);
        let (_, removed) = run_pr_pass(repo.path(), &spec, &idle_open_opts());
        assert!(removed.is_empty(), "{spec:?}");
    }
}

#[test]
#[serial]
fn test_resolve_idle_open_secs_precedence() {
    std::env::remove_var(WORKTREE_REAPER_IDLE_OPEN_ENV);
    let cfg = |v| WorktreeReaperConfig {
        idle_open_secs: v,
        ..WorktreeReaperConfig::default()
    };
    assert_eq!(resolve_idle_open_secs(&cfg(None)), Some(DEFAULT_IDLE_OPEN_SECS));
    assert_eq!(resolve_idle_open_secs(&cfg(Some(60))), Some(60));
    assert_eq!(resolve_idle_open_secs(&cfg(Some(0))), None, "0 disables");
    std::env::set_var(WORKTREE_REAPER_IDLE_OPEN_ENV, "30");
    assert_eq!(resolve_idle_open_secs(&cfg(Some(60))), Some(30));
    std::env::set_var(WORKTREE_REAPER_IDLE_OPEN_ENV, "0");
    assert_eq!(resolve_idle_open_secs(&cfg(Some(60))), None);
    std::env::remove_var(WORKTREE_REAPER_IDLE_OPEN_ENV);
}

#[test]
fn test_config_reads_idle_open_secs() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"worktreeReaper": {"idleOpenSecs": 3600}}}"#);
    assert_eq!(read_worktree_reaper_config(tmp.path()).idle_open_secs, Some(3600));
}
