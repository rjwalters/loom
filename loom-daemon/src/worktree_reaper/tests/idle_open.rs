//! Issue #12458: the reaper's treatment of an **open** issue / PR whose
//! worktree has sat idle — reapable once pushed and clean **and** a remote
//! backup of its whole state has been verified, kept otherwise.
//!
//! A sibling of `worktree_reaper.rs`'s own `mod tests` (whose fixtures it
//! inherits through `use super::*`) because that file is at its file-size
//! ratchet baseline.

use super::*;
use std::process::Command;

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

/// Turns the #8413 mtime veto off for one test and restores it after.
///
/// `RemoveWithBackup` gets the same registry-independent veto as the
/// quarantine route (a write inside the activity window keeps the worktree),
/// and every fixture here was written milliseconds ago. In production the idle
/// gate has already established `idleOpenSecs` of silence, so the veto only
/// matters to a test that wants to reach the backup step. The tests that use
/// it are `#[serial]`.
struct WindowOff(Option<String>);

fn window_off() -> WindowOff {
    let env = crate::worktree_activity::ACTIVITY_WINDOW_ENV;
    let previous = std::env::var(env).ok();
    std::env::set_var(env, "0");
    WindowOff(previous)
}

impl Drop for WindowOff {
    fn drop(&mut self) {
        let env = crate::worktree_activity::ACTIVITY_WINDOW_ENV;
        match self.0.take() {
            Some(value) => std::env::set_var(env, value),
            None => std::env::remove_var(env),
        }
    }
}

#[test]
#[serial]
fn test_idle_open_issue_worktree_is_reaped_after_a_verified_backup() {
    let repo = make_repo(&[(700, true)]);
    let _window = window_off();
    let (report, removed) = run_pass(repo.path(), &open_issue(true), &idle_open_opts());
    assert_eq!(removed, vec![700]);
    assert_eq!(report.removed, vec![700]);
    assert_eq!(report.backed_up.len(), 1, "the backup is recorded with the removal");
    assert_eq!(report.backed_up[0].0, 700);
    assert!(report.backed_up[0]
        .1
        .ref_name
        .starts_with("refs/loom/backup/issue-700/"));
    assert!(report.summary().contains("backed_up=1"));
}

#[test]
#[serial]
fn test_idle_open_issue_is_kept_when_the_backup_fails() {
    // The decisive negative: every gate says reap, the remote says no.
    let repo = make_repo(&[(708, true)]);
    let _window = window_off();
    let spec = ProbeSpec {
        backup_ok: false,
        ..open_issue(true)
    };
    let (report, removed) = run_pass(repo.path(), &spec, &idle_open_opts());
    assert!(removed.is_empty(), "the remover must not run without a verified backup");
    assert!(report.removed.is_empty() && report.failed.is_empty());
    assert!(report.backed_up.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].0, 708);
    assert!(
        report.skipped[0].1.starts_with("remote backup failed: "),
        "{:?}",
        report.skipped
    );
    assert_eq!(
        crate::observability::ops::reaper::preserve_reason(&report.skipped[0].1),
        "backup_failed"
    );
}

#[test]
#[serial]
fn test_a_closed_merged_issue_is_still_reaped_without_a_backup() {
    // The backup is the open-item contract only; the closed/merged path is
    // unchanged (and the scripted backup would fail here if it were asked).
    let repo = make_repo(&[(709, true)]);
    let spec = ProbeSpec {
        backup_ok: false,
        ..ProbeSpec::default()
    };
    let (report, removed) = run_pass(repo.path(), &spec, &idle_open_opts());
    assert_eq!(removed, vec![709]);
    assert!(report.backed_up.is_empty());
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
fn test_idle_open_pr_worktree_is_reaped_after_a_backup_and_dirty_unpushed_or_unbacked_is_kept() {
    let repo = make_pr_repo(&[(710, true)]);
    let _window = window_off();
    let spec = ProbeSpec {
        pr_status: PrStatus::Open,
        idle: true,
        ..ProbeSpec::default()
    };
    let (report, removed) = run_pr_pass(repo.path(), &spec, &idle_open_opts());
    assert_eq!(removed, vec![710]);
    assert_eq!(report.backed_up.len(), 1);
    assert!(report.backed_up[0]
        .1
        .ref_name
        .starts_with("refs/loom/backup/pr-710/"));

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
        ProbeSpec {
            backup_ok: false,
            ..spec.clone()
        },
    ] {
        let repo = make_pr_repo(&[(711, true)]);
        let (report, removed) = run_pr_pass(repo.path(), &spec, &idle_open_opts());
        assert!(removed.is_empty(), "{spec:?}");
        assert!(report.backed_up.is_empty(), "{spec:?}");
    }
}

#[test]
#[serial]
fn test_the_reaper_span_counts_backups_and_carries_their_refs() {
    let repo = make_repo(&[(712, true)]);
    let _window = window_off();
    let ((report, ()), captured) = crate::observability::ops::capture::capture(|| {
        let (report, _) = run_pass(repo.path(), &open_issue(true), &idle_open_opts());
        log_report(repo.path(), &report, 0);
        (report, ())
    });
    assert_eq!(report.removed, vec![712]);
    assert_eq!(captured.spans.len(), 1);
    let a = &captured.spans[0].attributes;
    assert_eq!(a["loom.reaper.backed_up"], "1");
    assert_eq!(a["loom.reaper.backup_refs"], report.backed_up[0].1.ref_name);
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

// ===================================================================
// End to end: a real worktree, the real backup, the real remover
// ===================================================================

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Loom Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "Loom Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A primary checkout `repo` with a bare `origin`, and a Loom-managed worktree
/// at `.loom/worktrees/issue-900` on `feature/issue-900`, pushed — plus one
/// untracked file in it that the scripted `unsaved_work` probe will *miss*,
/// which is exactly the gap the backup exists to close.
struct Live {
    _tmp: tempfile::TempDir,
    bare: PathBuf,
    repo: PathBuf,
    worktree: PathBuf,
}

fn live_repo() -> Live {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("origin.git");
    let repo = tmp.path().join("repo");
    git(tmp.path(), &["init", "-q", "--bare", "origin.git"]);
    git(tmp.path(), &["init", "-q", "-b", "main", "repo"]);
    fs::write(repo.join("README.md"), "hello\n").unwrap();
    fs::write(repo.join(".gitignore"), ".loom/\n.loom-managed\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&repo, &["push", "-q", "-u", "origin", "main"]);
    let worktree = repo.join(".loom/worktrees/issue-900");
    fs::create_dir_all(repo.join(".loom/worktrees")).unwrap();
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/issue-900",
            worktree.to_str().unwrap(),
            "main",
        ],
    );
    fs::write(worktree.join(".loom-managed"), "").unwrap();
    fs::write(worktree.join("work.rs"), "fn work() {}\n").unwrap();
    git(&worktree, &["add", "work.rs"]);
    git(&worktree, &["commit", "-q", "-m", "work"]);
    git(&worktree, &["push", "-q", "-u", "origin", "feature/issue-900"]);
    fs::write(worktree.join("notes.txt"), "the probe missed me\n").unwrap();
    Live {
        _tmp: tmp,
        bare,
        repo,
        worktree,
    }
}

/// Drive the issue-keyed pass over `live` with the open-issue scripted probes
/// (`unsaved_work` deliberately answering `false`), the **real** remote backup
/// and the **real** `cleanup_worktree` remover.
fn run_live_pass(live: &Live) -> ReapReport {
    let active = HashSet::new();
    let in_use_marker = |_: &Path| None;
    let processes_using = |_: &Path| Vec::new();
    let editable_installs = |_: &Path| Vec::new();
    let is_managed = |p: &Path| clean::is_loom_managed(p);
    let is_registered_worktree = |_: &Path| true;
    let issue_state = |_: u32| "OPEN".to_string();
    let issue_closed_at = |_: u32| None;
    let pr_status = |_: u32| PrStatus::Open;
    let branch_reachable = |_: u32| true;
    let uncommitted = |_: &Path| false;
    let probes = WorktreeProbes {
        active_issues: &active,
        in_use_marker: &in_use_marker,
        processes_using: &processes_using,
        editable_installs: &editable_installs,
        is_managed: &is_managed,
        is_registered_worktree: &is_registered_worktree,
        issue_state: &issue_state,
        issue_closed_at: &issue_closed_at,
        pr_status: &pr_status,
        branch_reachable_from_remotes: &branch_reachable,
        uncommitted: &uncommitted,
        unsaved_work: &|_: &Path| false,
        idle_for: &|_: &Path, _: i64| true,
        now: Utc::now(),
    };
    let repo_root = live.repo.as_path();
    // The production wrapper: the real push plus the ledger line.
    let backup =
        |path: &Path, issue: u32| backup_open_worktree(repo_root, path, &format!("issue-{issue}"));
    let remover = |path: &Path, issue: u32| {
        clean::cleanup_worktree(repo_root, path, issue, false, "worktree_reaper_test").is_ok()
    };
    // The fixture's writes are seconds old; in production the idle gate has
    // already established 24 h of silence, so the #8413 mtime veto is moot.
    let _window = window_off();
    reap_worktrees(repo_root, &idle_open_opts(), &probes, &|_, _| None, &backup, &remover)
}

#[test]
#[serial]
fn test_live_idle_open_worktree_is_removed_only_after_the_remote_holds_its_whole_state() {
    let live = live_repo();
    let tip = git(&live.worktree, &["rev-parse", "HEAD"]);

    let report = run_live_pass(&live);

    assert_eq!(report.removed, vec![900], "{:?}", report.skipped);
    assert!(!live.worktree.exists(), "the worktree directory is gone");
    assert_eq!(report.backed_up.len(), 1);
    let record = &report.backed_up[0].1;
    assert_eq!(record.head, tip);
    assert_eq!(record.branch.as_deref(), Some("feature/issue-900"));
    assert!(record.captured_changes(), "notes.txt made the snapshot differ from HEAD");
    // The remote holds the snapshot, and the snapshot holds the file every
    // probe said was not there.
    assert_eq!(git(&live.bare, &["rev-parse", &record.ref_name]), record.sha);
    assert_eq!(
        git(&live.bare, &["show", &format!("{}:notes.txt", record.sha)]),
        "the probe missed me"
    );
    assert_eq!(git(&live.bare, &["rev-parse", &format!("{}^", record.sha)]), tip);
    // The branch on the remote was not moved, and the ledger names the ref.
    assert_eq!(git(&live.bare, &["rev-parse", "refs/heads/feature/issue-900"]), tip);
    let ledger = fs::read_to_string(live.repo.join(".loom/logs/worktree-removals.log")).unwrap();
    assert!(ledger.contains(&record.ref_name), "{ledger}");
    assert!(ledger.contains("classify_worktree=Remove"), "{ledger}");
}

#[test]
#[serial]
fn test_live_idle_open_worktree_survives_a_failed_push_untouched() {
    let live = live_repo();
    let missing = live.bare.with_file_name("gone.git");
    git(&live.repo, &["remote", "set-url", "origin", missing.to_str().unwrap()]);

    let report = run_live_pass(&live);

    assert!(report.removed.is_empty() && report.failed.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].1.starts_with("remote backup failed"), "{:?}", report.skipped);
    assert!(live.worktree.is_dir(), "nothing was removed");
    assert_eq!(
        fs::read_to_string(live.worktree.join("notes.txt")).unwrap(),
        "the probe missed me\n"
    );
    assert_eq!(git(&live.worktree, &["status", "--porcelain"]), "?? notes.txt");
    assert!(git(&live.repo, &["worktree", "list"]).contains("issue-900"));
    assert!(!live.repo.join(".loom/logs/worktree-removals.log").exists(), "no ledger line");
}
