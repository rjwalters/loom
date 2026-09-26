//! The lock must still be a lock when acquisition happens in a SUBPROCESS.
//!
//! The unit tests in `worktree_cli::lock` cannot see this class: they acquire
//! and release inside one live test process, so the recorded owner pid is
//! always alive and stale-PID recovery never fires.
//!
//! Driving the real binary twice is what exposed it. `loom-daemon
//! worktree-lock acquire` exits as soon as it prints the token, so recording
//! `std::process::id()` wrote a lock owned by an already-dead process — and
//! the very next acquire reclaimed it as stale. Mutual exclusion was gone, and
//! it looked like it worked, because acquisition always succeeded.
//!
//! This is the port-method lesson in miniature: a lock's owner must be the
//! process whose LIFETIME defines the critical section. In the shell that was
//! `worktree.sh` itself; after the port it is the caller, which must pass its
//! own pid.

use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_loom-daemon")
}

fn git_init(dir: &Path) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["init", "-q"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "git init must succeed");
}

/// Acquire via the CLI on behalf of `owner_pid`. Returns the token, or `None`
/// when the attempt timed out.
fn cli_acquire(repo: &Path, issue: u32, owner_pid: u32, timeout: &str) -> Option<String> {
    let out = Command::new(bin())
        .args(["worktree-lock", "acquire", "--issue"])
        .arg(issue.to_string())
        .arg("--owner-pid")
        .arg(owner_pid.to_string())
        .arg("--timeout")
        .arg(timeout)
        .args(["--poll", "0.02", "--repo"])
        .arg(repo)
        .output()
        .expect("run acquire");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        stdout
            .lines()
            .find_map(|l| l.strip_prefix("TOKEN=").map(str::to_string))
    } else {
        None
    }
}

fn cli_release(repo: &Path, token: &str) {
    let ok = Command::new(bin())
        .args(["worktree-lock", "release", "--token", token, "--repo"])
        .arg(repo)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "release always exits 0");
}

/// Write a live-shaped per-issue CLAIM lock (`sweep_registry::locks`'s
/// `owner.json`, the DIFFERENT, longer-lived lock `check-issue` reads —
/// see `worktree_cli::issue_lock`'s module docs) directly, bypassing
/// `acquire`/`release` above entirely.
fn write_issue_claim_lock(repo: &Path, issue: u32, owner_pid: u32, sweep_id: &str) {
    let dir = repo.join(".loom/locks").join(format!("issue-{issue}"));
    std::fs::create_dir_all(&dir).expect("mkdir issue lock dir");
    std::fs::write(
        dir.join("owner.json"),
        format!(
            r#"{{"issue": {issue}, "owner_pid": {owner_pid}, "acquired_at": "2026-01-01T00:00:00Z", "sweep_id": "{sweep_id}"}}"#
        ),
    )
    .expect("write owner.json");
}

/// `check-issue`, with `LOOM_SWEEP_ID` set to `caller_sweep_id` (or unset when
/// `None`). Returns the exit status.
fn cli_check_issue(
    repo: &Path,
    issue: u32,
    caller_sweep_id: Option<&str>,
) -> std::process::ExitStatus {
    let mut cmd = Command::new(bin());
    cmd.args(["worktree-lock", "check-issue", "--issue"])
        .arg(issue.to_string())
        .arg("--repo")
        .arg(repo);
    match caller_sweep_id {
        Some(id) => {
            cmd.env("LOOM_SWEEP_ID", id);
        }
        None => {
            cmd.env_remove("LOOM_SWEEP_ID");
        }
    }
    cmd.status().expect("run check-issue")
}

#[test]
fn a_lock_held_on_behalf_of_a_live_caller_excludes_a_second_acquisition() {
    let d = tempfile::tempdir().expect("tempdir");
    git_init(d.path());

    // The test process stands in for the long-lived caller — exactly what
    // `worktree.sh` is, and what `$$` refers to there.
    let holder = std::process::id();

    let first = cli_acquire(d.path(), 1, holder, "5").expect("first acquisition succeeds");

    // THE REGRESSION: before `--owner-pid`, this succeeded, because the first
    // CLI process had already exited and its pid read as dead.
    assert!(
        cli_acquire(d.path(), 2, holder, "1").is_none(),
        "a second acquisition must BLOCK while a live caller holds the lock — \
         if this passes, the lock is recording a pid that dies with the CLI"
    );

    cli_release(d.path(), &first);
    let second = cli_acquire(d.path(), 2, holder, "5").expect("free after release");
    cli_release(d.path(), &second);
}

#[test]
fn a_lock_whose_caller_really_died_is_still_reclaimable() {
    // The counterpart, so the fix above cannot be "never break any lock".
    let d = tempfile::tempdir().expect("tempdir");
    git_init(d.path());

    // A pid that is genuinely absent stands in for a crashed caller.
    let dead = 0x7fff_fffe;
    let stale = cli_acquire(d.path(), 1, dead, "5").expect("acquire as the doomed caller");

    let reclaimed = cli_acquire(d.path(), 2, std::process::id(), "5")
        .expect("a dead owner's lock is reclaimed");
    assert_ne!(stale, reclaimed, "reclaiming must mint a fresh token");

    cli_release(d.path(), &reclaimed);
}

#[test]
fn a_stale_token_from_a_previous_holder_cannot_release_the_current_lock() {
    // #6014 through the process boundary, not just the library.
    let d = tempfile::tempdir().expect("tempdir");
    git_init(d.path());
    let holder = std::process::id();

    let a = cli_acquire(d.path(), 1, holder, "5").expect("A");
    cli_release(d.path(), &a);
    let b = cli_acquire(d.path(), 2, holder, "5").expect("B");

    cli_release(d.path(), &a); // A's late, stale release

    assert!(
        d.path().join(".loom/locks/worktree-add").is_dir(),
        "B's lock must survive A's stale release"
    );
    cli_release(d.path(), &b);
    assert!(!d.path().join(".loom/locks/worktree-add").is_dir());
}

/// #8702: a dispatched sweep's own `worktree.sh` calls must not be refused
/// by the claim lock IT holds — through the real binary and a real
/// subprocess environment, the level at which `std::env::var("LOOM_SWEEP_ID")`
/// actually reads.
#[test]
fn check_issue_exempts_the_lock_holders_own_sweep_id() {
    let d = tempfile::tempdir().expect("tempdir");
    git_init(d.path());

    // The test process stands in for the daemon's live sweep child.
    write_issue_claim_lock(d.path(), 42, std::process::id(), "sweep-42-self");

    let status = cli_check_issue(d.path(), 42, Some("sweep-42-self"));
    assert!(
        status.success(),
        "the sweep that holds the lock must not be refused by its own claim (#8702)"
    );
}

/// The counterpart, so the fix above cannot be "never refuse anything": a
/// caller naming a DIFFERENT sweep, or naming none at all (#8553's original
/// incident — no env var was consulted), still gets refused.
#[test]
fn check_issue_still_refuses_a_foreign_or_unset_sweep_id() {
    let d = tempfile::tempdir().expect("tempdir");
    git_init(d.path());
    write_issue_claim_lock(d.path(), 43, std::process::id(), "sweep-43-live");

    assert!(
        !cli_check_issue(d.path(), 43, Some("sweep-43-someone-else")).success(),
        "a different LOOM_SWEEP_ID must not be treated as the lock's owner"
    );
    assert!(
        !cli_check_issue(d.path(), 43, None).success(),
        "an unset LOOM_SWEEP_ID must still refuse (#8553)"
    );
}
