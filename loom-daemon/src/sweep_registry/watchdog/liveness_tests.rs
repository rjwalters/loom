//! Issue #8413: the mid-build watchdog's destructive path, from the point of
//! view of a worker the daemon has **no registry record for**.
//!
//! Kept beside (not inside) `watchdog/tests.rs` because that file is at its
//! file-size ratchet baseline — see `.loom/docs/file-size-policy.md`. The
//! hazards documented at the top of `tests.rs` (shared-process env mutation,
//! real `git` children — run under `cargo nextest run`) apply here verbatim,
//! and more so: these tests set `LOOM_INFLIGHT_DIR`, so they are `#[serial]`.

use super::*;
use crate::inflight::{self, ClaimOutcome, Registration};
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use tempfile::tempdir;

/// Register an in-flight claim against `tree`, exactly as an in-session
/// builder's `loom-daemon inflight claim --command … --tree <wt> --pid $$` does.
fn claim_tree(store: &Path, command: &str, tree: &Path, pid: u32) {
    let tree = tree.to_string_lossy().to_string();
    let reg = Registration {
        fingerprint: inflight::fingerprint(command, &tree, ""),
        command: command.to_string(),
        tree: inflight::normalize_tree(&tree),
        branch: String::new(),
        pid,
        agent: "in-session builder (Task tool)".to_string(),
        started_at: chrono::Utc::now(),
    };
    assert!(matches!(
        inflight::claim_in(store, &reg, Duration::from_secs(3600)),
        ClaimOutcome::Claimed(_)
    ));
}

fn git_stdout(wt: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// AC (#8413): a worktree held by an in-session builder — a live in-flight
/// claim, and none of the four pre-existing registry-visible signals — is NOT
/// `git reset --hard`ed by the mid-build watchdog, and the refusal does not
/// consume the single recovery retry.
///
/// This is the incident's shape: no claim-lock (the builder is not a daemon
/// sweep), no `.loom-in-use` marker, no `index.lock`, and no long-lived process
/// with a cwd inside the worktree — its shell commands are one-shot subshells
/// that have already exited by the time the watchdog looks.
#[test]
#[serial]
fn an_in_session_builders_inflight_claim_refuses_the_midbuild_reset() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);
    let wt = make_quiet_dirty_git_worktree(&mut reg, ws, 8413);
    insert_terminal_issue(&mut reg, "sweep-issue-8413-dead", 8413, None);

    let store = ws.join("inflight-store");
    std::env::set_var(inflight::INFLIGHT_DIR_ENV, &store);
    claim_tree(&store, "cargo build --workspace", &wt, std::process::id());

    let evidence = reg.worktree_in_use(8413);
    assert!(
        evidence.iter().any(
            |e| matches!(e, WorktreeUseEvidence::InflightClaim(s) if s.contains("cargo build"))
        ),
        "the in-flight claim must register as live-use evidence: {evidence:?}"
    );

    assert_eq!(
        reg.midbuild_watchdog_once(),
        0,
        "no reset while an in-flight claim covers the worktree"
    );
    assert!(
        wt.join("dirty.txt").exists(),
        "the in-session builder's uncommitted mid-build work MUST survive (#8413)"
    );
    assert!(
        !reg.midbuild_retried.contains(&8413),
        "an in-use refusal must NOT consume the single recovery retry"
    );

    std::env::remove_var(inflight::INFLIGHT_DIR_ENV);
}

/// AC (#8413), the implicit path: a worktree being written to right now — the
/// observable signature of a live `cargo` compile, whose output lands only in
/// `target/` — is NOT reset, even though the builder registered nothing at all.
///
/// This is the incident's shape without the cooperation the test above assumes:
/// no claim, no marker, no lock, no visible process. The fixture pins this
/// registry's activity window off (see `make_quiet_dirty_git_worktree`); this
/// test turns it back on — at the production default — for the same registry,
/// with no process-global state involved either way (#8487).
#[test]
#[serial]
fn a_live_compile_writing_only_into_target_refuses_the_midbuild_reset() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);
    let wt = make_quiet_dirty_git_worktree(&mut reg, ws, 8417);
    insert_terminal_issue(&mut reg, "sweep-issue-8417-dead", 8417, None);

    // A compile's writes: `target/` only, nothing the tracked tree can see.
    std::fs::create_dir_all(wt.join("target/debug")).unwrap();
    std::fs::write(wt.join("target/debug/partial.rlib"), "…").unwrap();
    reg.set_activity_window(Some(Duration::from_secs(
        crate::worktree_activity::DEFAULT_ACTIVITY_WINDOW_MINUTES * 60,
    )));

    let evidence = reg.worktree_in_use(8417);
    assert!(
        evidence
            .iter()
            .any(|e| matches!(e, WorktreeUseEvidence::RecentWrite { .. })),
        "a running build must register as live-use evidence: {evidence:?}"
    );
    assert_eq!(reg.midbuild_watchdog_once(), 0, "no reset while a build is writing");
    assert!(
        wt.join("dirty.txt").exists(),
        "the in-session builder's uncommitted mid-build work MUST survive (#8413)"
    );
    assert!(
        !reg.midbuild_retried.contains(&8417),
        "an in-use refusal must NOT consume the single recovery retry"
    );

    // …and once the worktree goes quiet, the same dead sweep is recovered.
    reg.set_activity_window(Some(Duration::ZERO));
    assert_eq!(reg.midbuild_watchdog_once(), 1, "recovery resumes once the writes stop");
}

/// The positive control for the test above, and the other half of the gate:
/// once the claimant releases (or its process dies), the very same worktree is
/// recovered normally. Without this the veto could be "always refuse".
#[test]
#[serial]
fn recovery_resumes_once_the_inflight_claim_is_released() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);
    let wt = make_quiet_dirty_git_worktree(&mut reg, ws, 8415);
    insert_terminal_issue(&mut reg, "sweep-issue-8415-dead", 8415, None);

    let store = ws.join("inflight-store");
    std::env::set_var(inflight::INFLIGHT_DIR_ENV, &store);
    claim_tree(&store, "cargo test --workspace", &wt, std::process::id());
    assert_eq!(reg.midbuild_watchdog_once(), 0, "refused while the claim is live");

    // The builder finishes: `loom-daemon inflight release` removes the entry.
    let fingerprint = inflight::fingerprint("cargo test --workspace", &wt.to_string_lossy(), "");
    assert!(inflight::release_in(&store, &fingerprint, None, true));

    assert_eq!(reg.midbuild_watchdog_once(), 1, "recovery resumes once the claim is released");
    assert!(!wt.join("dirty.txt").exists(), "the recovery did reset the worktree");

    std::env::remove_var(inflight::INFLIGHT_DIR_ENV);
}

/// AC (#8413): a reset the watchdog *does* perform leaves a recoverable stash
/// ref — `loom-quarantine:`-labelled, applyable — instead of only a log line.
#[test]
#[serial]
fn a_performed_reset_leaves_a_recoverable_quarantine_stash() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);
    let wt = make_quiet_dirty_git_worktree(&mut reg, ws, 8416);
    insert_terminal_issue(&mut reg, "sweep-issue-8416-dead", 8416, None);

    assert_eq!(reg.midbuild_watchdog_once(), 1, "a genuinely dead sweep is still recovered");
    assert!(!wt.join("dirty.txt").exists(), "the reset discarded the working-tree state");

    let listed = git_stdout(&wt, &["stash", "list"]);
    assert!(
        listed.contains("loom-quarantine:") && listed.contains("issue=8416"),
        "the reset must leave a labelled quarantine stash: {listed:?}"
    );

    let sha = git_stdout(&wt, &["rev-parse", "refs/stash"])
        .trim()
        .to_string();
    assert!(!sha.is_empty(), "the quarantine stash has a commit sha to log");
    let applied = Command::new("git")
        .arg("-C")
        .arg(&wt)
        .args(["stash", "apply", &sha])
        .output()
        .unwrap();
    assert!(
        applied.status.success(),
        "`git stash apply <sha>` must restore the discarded work: {}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert!(
        wt.join("dirty.txt").exists(),
        "the destroyed mid-build edit is recoverable from the stash (#8413)"
    );
}
