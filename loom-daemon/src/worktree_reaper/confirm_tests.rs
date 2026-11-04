//! W6 PR2: the reap loop asks for the pre-removal confirm exactly once per
//! worktree it is about to remove, before the quarantine and before the
//! removal, and a confirm that says keep stops both.
#![allow(clippy::unwrap_used)]

use std::cell::RefCell;

use serial_test::serial;

use super::*;
use crate::worktree_activity::ACTIVITY_WINDOW_ENV;

/// `issue-7` is removable, `issue-8` removable after a quarantine, `issue-9`
/// preserved by a gate. Returns the report and what ran, in order. The
/// activity window is off: these directories were written a moment ago.
fn pass_over(confirm: &dyn Fn(u32) -> Option<String>) -> (ReapReport, Vec<String>) {
    let root = tempfile::tempdir().unwrap();
    for n in [7, 8, 9] {
        std::fs::create_dir_all(root.path().join(format!(".loom/worktrees/issue-{n}"))).unwrap();
    }
    let previous = std::env::var(ACTIVITY_WINDOW_ENV).ok();
    std::env::set_var(ACTIVITY_WINDOW_ENV, "0");
    let ran = RefCell::new(Vec::new());
    let report = reap_worktrees_generic(
        root.path(),
        &crate::worktree_ops::naming::issue_from_worktree,
        &|_, n| match n {
            7 => WorktreeDecision::Remove,
            8 => WorktreeDecision::RemoveWithQuarantine,
            _ => WorktreeDecision::SkipPrOpen,
        },
        &|_, n| {
            ran.borrow_mut().push(format!("quarantine {n}"));
            Some("deadbeef".to_string())
        },
        &|_, n| {
            ran.borrow_mut().push(format!("backup {n}"));
            Err("no backup route in this test".to_string())
        },
        &|_, n| {
            ran.borrow_mut().push(format!("remove {n}"));
            true
        },
        &|n| {
            ran.borrow_mut().push(format!("confirm {n}"));
            confirm(n)
        },
    );
    match previous {
        Some(value) => std::env::set_var(ACTIVITY_WINDOW_ENV, value),
        None => std::env::remove_var(ACTIVITY_WINDOW_ENV),
    }
    (report, ran.into_inner())
}

#[test]
#[serial]
fn every_removal_is_confirmed_once_and_first() {
    let (report, ran) = pass_over(&|_| None);
    assert_eq!(
        ran,
        [
            "confirm 7",
            "remove 7",
            "confirm 8",
            "quarantine 8",
            "remove 8"
        ],
        "one confirm per removal, before anything destructive; none for a kept worktree"
    );
    assert_eq!(report.removed, vec![7, 8]);
    assert_eq!(report.skipped, vec![(9, "PR still open".to_string())]);
}

#[test]
#[serial]
fn a_confirm_that_says_keep_stops_the_quarantine_and_the_removal() {
    let (report, ran) = pass_over(&|n| Some(format!("fresh read disagreed about #{n}")));
    assert_eq!(ran, ["confirm 7", "confirm 8"], "nothing was quarantined or removed");
    assert!(report.removed.is_empty() && report.failed.is_empty(), "{report:?}");
    assert_eq!(
        report.skipped,
        vec![
            (7, "fresh read disagreed about #7".to_string()),
            (8, "fresh read disagreed about #8".to_string()),
            (9, "PR still open".to_string()),
        ]
    );
}

#[test]
#[serial]
fn only_the_disagreeing_worktree_is_kept() {
    let (report, ran) = pass_over(&|n| (n == 8).then(|| "no".to_string()));
    assert_eq!(ran, ["confirm 7", "remove 7", "confirm 8"]);
    assert_eq!(report.removed, vec![7]);
}

/// #13442: a successful pre-removal remote backup is the reaper doing its job,
/// so it logs at INFO (restore recipe kept); a failed backup stays WARN.
#[test]
#[serial]
fn backup_success_logs_info_and_backup_failure_logs_warn() {
    let run = |ok: bool| {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".loom/worktrees/issue-7")).unwrap();
        let previous = std::env::var(ACTIVITY_WINDOW_ENV).ok();
        std::env::set_var(ACTIVITY_WINDOW_ENV, "0");
        let logs = crate::test_log_capture::capture_logs(|| {
            let _ = reap_worktrees_generic(
                root.path(),
                &crate::worktree_ops::naming::issue_from_worktree,
                &|_, _| WorktreeDecision::RemoveWithBackup,
                &|_, _| None,
                &|_, _| {
                    if ok {
                        Ok(crate::worktree_ops::open_backup::BackupRecord {
                            remote: "origin".to_string(),
                            ref_name: "refs/loom/backup/issue-7/x".to_string(),
                            sha: "b".repeat(40),
                            head: "b".repeat(40),
                            branch: Some("feature/issue-7".to_string()),
                        })
                    } else {
                        Err("push refused".to_string())
                    }
                },
                &|_, _| true,
                &|_| None,
            );
        });
        match previous {
            Some(value) => std::env::set_var(ACTIVITY_WINDOW_ENV, value),
            None => std::env::remove_var(ACTIVITY_WINDOW_ENV),
        }
        logs
    };
    let ok_logs = run(true);
    let backup = ok_logs
        .iter()
        .find(|(_, m)| m.contains("pushed a remote backup before removal"))
        .expect("backup success line is still logged");
    assert_eq!(backup.0, log::Level::Info);
    assert!(backup.1.contains("restore:"), "restore recipe kept: {}", backup.1);

    let fail_logs = run(false);
    let failed = fail_logs
        .iter()
        .find(|(_, m)| m.contains("remote backup failed"))
        .expect("failure line is logged");
    assert_eq!(failed.0, log::Level::Warn);
}
