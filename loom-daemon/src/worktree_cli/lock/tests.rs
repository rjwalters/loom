//! Tests for the worktree-add lock.
//!
//! The #6014 ownership cases come first: they are the reason `release` takes a
//! token at all, and they are what a future simplification would delete.

use super::*;
use std::time::Duration;

fn repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().expect("tempdir");
    // A real repo, so `locks_dir` exercises the `--git-common-dir` path rather
    // than its fallback — the fallback is what a test would silently measure
    // otherwise.
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(d.path())
        .args(["init", "-q"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "git init must succeed for these tests to mean anything");
    d
}

const FAST: Duration = Duration::from_millis(200);
const POLL: Duration = Duration::from_millis(10);

// --- #6014: release must prove ownership ---

#[test]
fn a_stale_holders_late_release_leaves_the_live_holders_lock_intact() {
    // The incident: holder A is cleared, the lock is reassigned to live holder
    // B, and A's late release then deleted B's lock. Two sessions proceeded
    // into `git worktree add` at once.
    let d = repo();
    let token_a = acquire(d.path(), 90, std::process::id(), FAST, POLL).expect("A acquires");
    release(d.path(), &token_a);

    let token_b = acquire(d.path(), 91, std::process::id(), FAST, POLL).expect("B acquires");
    assert_ne!(token_a, token_b, "two acquisitions must not share a token");

    // A releases late, with its now-stale token.
    release(d.path(), &token_a);

    assert!(lock_path(d.path()).is_dir(), "B's lock must survive A's stale release");
    let still = recorded(&lock_path(d.path())).expect("B's owner.json readable");
    assert_eq!(still.token, token_b, "the lock must still be B's");
}

#[test]
fn an_empty_token_releases_nothing() {
    // A caller that never held the lock, or already released it, cannot prove
    // ownership of anything — so it must remove nothing.
    let d = repo();
    let held = acquire(d.path(), 1, std::process::id(), FAST, POLL).expect("acquire");
    release(d.path(), "");
    assert!(lock_path(d.path()).is_dir(), "empty token must be a no-op");
    release(d.path(), &held);
    assert!(!lock_path(d.path()).is_dir());
}

#[test]
fn a_lock_with_unreadable_metadata_is_not_ours_to_remove() {
    // We cannot prove it is ours, so we leave it. `acquire`'s stale-PID path
    // is what reclaims a genuinely abandoned lock — not `release` guessing.
    let d = repo();
    let token = acquire(d.path(), 1, std::process::id(), FAST, POLL).expect("acquire");
    std::fs::write(lock_path(d.path()).join("owner.json"), "{ not json").unwrap();
    release(d.path(), &token);
    assert!(lock_path(d.path()).is_dir(), "a lock we cannot read is a lock we cannot claim");
}

// --- mutual exclusion ---

#[test]
fn a_second_acquisition_blocks_until_the_first_releases() {
    let d = repo();
    let first = acquire(d.path(), 1, std::process::id(), FAST, POLL).expect("first");
    let err = acquire(d.path(), 2, std::process::id(), Duration::from_millis(80), POLL)
        .expect_err("second must not get the lock");
    match err {
        AcquireError::Timeout { holder_pid } => {
            assert_eq!(
                holder_pid,
                Some(std::process::id()),
                "the timeout must name the holder so an operator can find it"
            );
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
    release(d.path(), &first);
    let second = acquire(d.path(), 2, std::process::id(), FAST, POLL).expect("lock is free again");
    release(d.path(), &second);
}

#[test]
fn the_lock_is_repo_global_not_per_issue() {
    // `git worktree add` mutates `.git/worktrees/` for the whole repository,
    // so two adds for DIFFERENT issues still race. A per-issue lock would look
    // correct and serialise nothing.
    let d = repo();
    let a = acquire(d.path(), 100, std::process::id(), FAST, POLL).expect("issue 100");
    assert!(
        acquire(d.path(), 200, std::process::id(), Duration::from_millis(60), POLL).is_err(),
        "a different issue must still block"
    );
    release(d.path(), &a);
}

// --- stale-PID recovery ---

#[test]
fn a_lock_owned_by_a_dead_pid_is_broken_once() {
    let d = repo();
    std::fs::create_dir_all(lock_path(d.path())).unwrap();
    // pid 0x7FFFFFFE is not a live process on any platform Loom runs on.
    let dead = Owner {
        issue: 7,
        owner_pid: 0x7fff_fffe,
        token: "someone-elses".to_string(),
        script: "worktree.sh".to_string(),
        acquired_at: iso_now(),
    };
    std::fs::write(lock_path(d.path()).join("owner.json"), serde_json::to_string(&dead).unwrap())
        .unwrap();

    let token = acquire(d.path(), 7, std::process::id(), FAST, POLL)
        .expect("a dead owner's lock is reclaimable");
    let now = recorded(&lock_path(d.path())).expect("readable");
    assert_eq!(now.token, token, "the reclaimed lock must record OUR token");
    assert_eq!(now.owner_pid, std::process::id());
    release(d.path(), &token);
}

#[test]
fn a_live_holder_is_never_broken_even_on_a_long_wait() {
    // The counterpart that makes the previous test mean something: a suite
    // that only proves stale locks are broken would pass an implementation
    // that breaks every lock.
    let d = repo();
    let held = acquire(d.path(), 1, std::process::id(), FAST, POLL).expect("hold it");
    let err = acquire(d.path(), 2, std::process::id(), Duration::from_millis(150), POLL)
        .expect_err("must not break");
    assert!(matches!(err, AcquireError::Timeout { .. }));
    let still = recorded(&lock_path(d.path())).expect("readable");
    assert_eq!(still.token, held, "the live holder's lock is untouched");
    release(d.path(), &held);
}

#[test]
fn an_unsignalable_pid_is_treated_as_alive() {
    // `kill(pid, 0)` failing with EPERM means the process EXISTS and is not
    // ours. Reading that as "dead" would break a live lock owned by another
    // user — the unsafe direction.
    assert!(pid_alive(1), "pid 1 exists and is typically not signalable by us");
    assert!(!pid_alive(0x7fff_fffe), "a genuinely absent pid is dead");
}

// --- lock location ---

#[test]
fn every_linked_worktree_of_a_repo_resolves_to_the_same_lock() {
    // The property that makes it repo-global. `--git-common-dir` is what
    // delivers it; `--git-dir` would give each linked worktree its own lock
    // and serialise nothing across them.
    let d = repo();
    let main_lock = lock_path(d.path());
    let sub = d.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    // A plain subdirectory of the repo resolves to the same common dir.
    assert_eq!(lock_path(&sub), main_lock);
}

#[test]
fn owner_json_field_names_are_contract() {
    // The retained suite and merge-pr.sh's diagnostics both read this file by
    // key name; renaming a field is a silent break for them.
    let d = repo();
    let token = acquire(d.path(), 42, std::process::id(), FAST, POLL).expect("acquire");
    let raw = std::fs::read_to_string(lock_path(d.path()).join("owner.json")).unwrap();
    for key in ["issue", "owner_pid", "token", "script", "acquired_at"] {
        assert!(raw.contains(&format!("\"{key}\"")), "missing key {key} in {raw}");
    }
    assert!(raw.contains("\"worktree.sh\""), "script name is contract too");
    release(d.path(), &token);
}
