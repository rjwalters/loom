//! Unit tests for the orphan guard (#8195 slice 5).
//!
//! # Why these run out-of-process
//!
//! [`super::run`] reads the process's **cwd** (`git rev-parse
//! --git-common-dir`, `git worktree list --porcelain` and `git worktree
//! prune` are all invoked with no `-C`, exactly as the shell invoked them) and
//! the `PWD` environment variable. Every `#[test]` in this crate links into
//! one multi-threaded binary, so a `set_current_dir` here would be visible to
//! every other test running at the same time.
//!
//! So each case forks the test binary's own `loom-daemon`… except there isn't
//! one to fork from a unit test. Instead the cwd-dependent pieces are tested
//! through the pure functions they were factored into
//! ([`super::absolutize`], [`super::remove_path`], [`super::remove_stale_locks`]),
//! and the end-to-end cwd behaviour — including the space-in-path and
//! symlinked-path regressions — is covered by
//! `tests/worktree_cleanup_differential.rs`, which runs the real binary in the
//! real cwd against the retired shell.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use super::*;

fn tmpdir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "loom-cleanup-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("create tmpdir");
    fs::canonicalize(&base).expect("canonicalize tmpdir")
}

fn quiet() -> Reporter {
    Reporter {
        out: Out::new(false),
        quiet: true,
    }
}

// ---------------------------------------------------------------------------
// 1. Stale lock sweep
// ---------------------------------------------------------------------------

#[test]
fn removes_all_three_stale_locks_and_reports_cleaned() {
    let dir = tmpdir("locks");
    let admin = dir.join("worktrees").join("issue-42");
    fs::create_dir_all(&admin).unwrap();
    for lock in STALE_LOCKS {
        fs::write(admin.join(lock), b"").unwrap();
    }

    assert!(remove_stale_locks(&admin, &quiet()));
    for lock in STALE_LOCKS {
        assert!(!admin.join(lock).exists(), "{lock} survived the stale-lock sweep");
    }
}

#[test]
fn absent_locks_are_not_a_reason_to_prune() {
    let dir = tmpdir("nolocks");
    let admin = dir.join("worktrees").join("issue-42");
    fs::create_dir_all(&admin).unwrap();

    assert!(
        !remove_stale_locks(&admin, &quiet()),
        "an admin dir with no locks must not report that anything was cleaned"
    );
}

#[test]
fn a_lock_that_is_a_directory_is_left_alone() {
    // `[[ -f ]]` is regular-files-only, so a directory named `index.lock` is
    // not a stale lock. Preserved: `rm -f` would have failed on it anyway, and
    // reaching for `rm -rf` here would put a second recursive delete in a
    // function that already has the dangerous one.
    let dir = tmpdir("dirlock");
    let admin = dir.join("worktrees").join("issue-42");
    fs::create_dir_all(admin.join("index.lock")).unwrap();

    assert!(!remove_stale_locks(&admin, &quiet()));
    assert!(admin.join("index.lock").is_dir());
}

// ---------------------------------------------------------------------------
// 2. `rm -rf` semantics
// ---------------------------------------------------------------------------

#[test]
fn an_orphan_symlink_is_unlinked_and_its_target_survives() {
    // The single worst outcome available to this function: following a
    // symlinked `issue-<N>` path would delete a tree outside the worktree root
    // entirely. `rm -rf` unlinks; `remove_dir_all` would follow.
    let dir = tmpdir("symlink-orphan");
    let real = dir.join("somewhere else");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("PRECIOUS.txt"), b"another agent's work").unwrap();

    let link = dir.join("issue-42");
    symlink(&real, &link).unwrap();

    assert!(remove_path(&link));
    assert!(!link.exists(), "the symlink itself should be gone");
    assert!(
        real.join("PRECIOUS.txt").is_file(),
        "remove_path followed the symlink and destroyed its target"
    );
}

#[test]
fn a_real_orphan_directory_is_removed_recursively() {
    let dir = tmpdir("real-orphan");
    let orphan = dir.join("issue-42");
    fs::create_dir_all(orphan.join("nested").join("deeper")).unwrap();
    fs::write(orphan.join("nested").join("leftover.txt"), b"debris").unwrap();

    assert!(remove_path(&orphan));
    assert!(!orphan.exists());
}

#[test]
fn a_vanished_path_is_not_a_reason_to_prune() {
    let dir = tmpdir("vanished");
    assert!(!remove_path(&dir.join("never-existed")));
}

// ---------------------------------------------------------------------------
// 3. The porcelain read — #7858/#7849, structurally
// ---------------------------------------------------------------------------

#[test]
fn a_porcelain_path_containing_spaces_is_read_whole() {
    // The #7858 bug in its read-only form: `awk '{print $2}'` yields
    // "/Users/me/My" for this line, misses the `grep -Fxq`, and the caller
    // `rm -rf`s a LIVE worktree. There is nothing to word-split here.
    let porcelain = "worktree /Users/me/My Repos/repo/.loom/worktrees/issue-42\n\
                     HEAD 0000000000000000000000000000000000000000\n\
                     branch refs/heads/feature/issue-42\n";
    let paths: Vec<PathBuf> = super::super::branch_delete::parse_worktree_porcelain(porcelain)
        .into_iter()
        .map(|(p, _)| p)
        .collect();

    assert_eq!(
        paths,
        vec![PathBuf::from(
            "/Users/me/My Repos/repo/.loom/worktrees/issue-42"
        )]
    );
}

#[test]
fn an_unresolvable_candidate_reads_as_unregistered() {
    // Matches the shell's `abs_wt=""` branch. A path that cannot be
    // canonicalized is not the path git reported.
    let dir = tmpdir("unresolvable");
    assert!(!is_registered(&dir.join("does-not-exist")));
}

// ---------------------------------------------------------------------------
// 4. Path resolution
// ---------------------------------------------------------------------------

#[test]
fn absolutize_leaves_an_absolute_path_alone() {
    assert_eq!(
        absolutize(Path::new("/Users/me/My Repos/repo")),
        PathBuf::from("/Users/me/My Repos/repo")
    );
}

#[test]
fn absolutize_drops_the_curdir_component_dirname_of_dotgit_produces() {
    // `git rev-parse --git-common-dir` answers `.git` from a repo root, so the
    // shell's `dirname "$git_common"` is `.` and `cd . && pwd` is the cwd. The
    // `.` must not survive into the path that gets printed in the warning.
    let resolved = absolutize(Path::new(""));
    assert!(resolved.is_absolute(), "{resolved:?} should be absolute");
    assert!(
        !resolved.to_string_lossy().contains("/./"),
        "{resolved:?} still carries a CurDir component"
    );
}

#[test]
fn absolutize_preserves_dotdot_rather_than_folding_it_lexically() {
    // Folding `..` lexically is wrong through a symlink, and bash's `cd` does
    // not do it either. Pinned so a later "simplification" has to argue with
    // this test rather than with a comment.
    let resolved = absolutize(Path::new("/a/b/../c"));
    assert_eq!(resolved, PathBuf::from("/a/b/../c"));
}

#[test]
fn logical_cwd_falls_back_to_the_physical_cwd_when_pwd_is_relative() {
    // `PWD` is read, not trusted: a relative value is not a cwd alias.
    // (Env mutation is confined to this one case and restored immediately;
    // the value it sets is rejected, so a racing reader sees the same answer
    // either way.)
    let physical = std::env::current_dir().unwrap();
    let saved = std::env::var_os("PWD");
    // SAFETY: single-statement window, and the value set is one this function
    // is required to reject.
    unsafe { std::env::set_var("PWD", "relative/nonsense") };
    let answer = logical_cwd();
    match saved {
        Some(v) => unsafe { std::env::set_var("PWD", v) },
        None => unsafe { std::env::remove_var("PWD") },
    }
    assert_eq!(answer, physical);
}

// ---------------------------------------------------------------------------
// 5. The quiet contract
// ---------------------------------------------------------------------------

#[test]
fn quiet_suppresses_rather_than_reroutes() {
    // Regression shape rather than an output capture: `Reporter::warning`
    // must not call through at all when quiet, because `worktree.sh --json`
    // has already pointed fd 1 at stderr and a rerouted line would appear on
    // stderr where the pre-port script emitted nothing.
    let r = Reporter {
        out: Out::new(false),
        quiet: true,
    };
    assert!(r.quiet, "the quiet flag must reach the reporter");
    // Exercising it must not panic and must produce no output; the
    // differential harness asserts the byte-level absence end-to-end.
    r.warning("this line must not appear anywhere");
}
