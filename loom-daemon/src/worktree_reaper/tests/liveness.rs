//! Issue #8413: the reaper's removal pass seen from a live worker the daemon
//! has **no registry record for** — plus the three dirty-past-grace cases
//! (#6653) whose preconditions that veto changed.
//!
//! A sibling of `worktree_reaper.rs`'s own `mod tests` (whose helpers it
//! inherits through `use super::*`) because that file is at its file-size
//! ratchet baseline — see `.loom/docs/file-size-policy.md`.
//!
//! # Why these tests pin the activity window
//!
//! `make_repo`'s worktree directories are created microseconds before the pass
//! runs, so their mtimes are always "recent" — which is precisely the signal
//! [`crate::worktree_activity::removal_veto`] now treats as a live worker for a
//! **dirty** worktree. A test that means "dirty, past grace, and *quiet*" must
//! therefore say so: `LOOM_WORKTREE_ACTIVITY_WINDOW_MINUTES=0` disables the
//! filesystem leg, leaving the in-flight-claim leg (which these cases do not
//! register) and reproducing the pre-#8413 preconditions exactly.
//!
//! These are `#[serial]` for that env var, like every other test in the parent
//! module.

use super::*;
use crate::inflight::{self, ClaimOutcome, Registration};
use crate::worktree_activity::ACTIVITY_WINDOW_ENV;
use std::time::Duration;

/// Run `body` with the filesystem-activity leg of the #8413 veto disabled,
/// restoring the previous value afterwards.
fn with_activity_gate_off<T>(body: impl FnOnce() -> T) -> T {
    let previous = std::env::var(ACTIVITY_WINDOW_ENV).ok();
    std::env::set_var(ACTIVITY_WINDOW_ENV, "0");
    let out = body();
    match previous {
        Some(value) => std::env::set_var(ACTIVITY_WINDOW_ENV, value),
        None => std::env::remove_var(ACTIVITY_WINDOW_ENV),
    }
    out
}

/// Register an in-flight claim against `tree` in `store`, as an in-session
/// builder's `loom-daemon inflight claim --tree <worktree> --pid $$` does.
fn claim_tree(store: &Path, command: &str, tree: &Path) {
    let tree_s = tree.to_string_lossy().to_string();
    let reg = Registration {
        fingerprint: inflight::fingerprint(command, &tree_s, ""),
        command: command.to_string(),
        tree: inflight::normalize_tree(&tree_s),
        branch: String::new(),
        pid: std::process::id(),
        agent: "in-session builder (Task tool)".to_string(),
        started_at: Utc::now(),
    };
    assert!(matches!(
        inflight::claim_in(store, &reg, Duration::from_secs(3600)),
        ClaimOutcome::Claimed(_)
    ));
}

// ---------------------------------------------------------------------------
// #6653 cases, relocated: dirty + past grace + QUIET
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn test_uncommitted_changes_past_grace_are_quarantined_then_reaped() {
    // #6653: the grace period already elapsed (the default spec's merge
    // timestamp is well in the past) — uncommitted changes no longer hold the
    // worktree forever, they get quarantine-stashed first. #8413 adds one
    // precondition: nothing may be actively writing into it.
    let repo = make_repo(&[(303, true)]);
    let spec = ProbeSpec {
        uncommitted: true,
        quarantine_ok: true,
        ..ProbeSpec::default()
    };
    let (report, removed, quarantined) =
        with_activity_gate_off(|| run_pass_full(repo.path(), &spec, &default_opts()));
    assert_eq!(removed, vec![303]);
    assert_eq!(report.removed, vec![303]);
    assert!(report.skipped.is_empty());
    assert_eq!(quarantined, vec![303]);
}

#[test]
#[serial]
fn test_uncommitted_changes_past_grace_block_the_reap_when_quarantine_fails() {
    // A failed (or no-op) `git stash push` must never be silently treated as
    // "safe to remove" — the worktree stays put.
    let repo = make_repo(&[(303, true)]);
    let spec = ProbeSpec {
        uncommitted: true,
        quarantine_ok: false,
        ..ProbeSpec::default()
    };
    let (report, removed, quarantined) =
        with_activity_gate_off(|| run_pass_full(repo.path(), &spec, &default_opts()));
    assert!(removed.is_empty());
    assert_eq!(quarantined, vec![303], "the quarantine attempt itself must still happen");
    assert!(report.skipped[0].1.contains("quarantine-stash failed"), "{:?}", report.skipped);
}

#[test]
#[serial]
fn test_no_pr_worktree_dirty_past_grace_is_quarantined_then_reaped() {
    let repo = make_repo(&[(324, true)]);
    let spec = ProbeSpec {
        pr_status: PrStatus::NoPr,
        issue_closed_at: Some("2020-01-01T00:00:00Z".to_string()),
        branch_reachable: true,
        uncommitted: true,
        quarantine_ok: true,
        ..ProbeSpec::default()
    };
    let (report, removed, quarantined) =
        with_activity_gate_off(|| run_pass_full(repo.path(), &spec, &default_opts()));
    assert_eq!(removed, vec![324]);
    assert_eq!(report.removed, vec![324]);
    assert!(report.skipped.is_empty());
    assert_eq!(quarantined, vec![324], "the dirt must be stashed before removal");
}

// ---------------------------------------------------------------------------
// #8413: the veto itself, exercised through a whole reap pass
// ---------------------------------------------------------------------------

/// AC (#8413): a worktree being written to right now — the observable
/// signature of a live compile, with no claim-lock, no `.loom-in-use` marker
/// and no process the reaper can see — is NOT removed, even though every forge
/// gate says it is reclaimable and its dirt would have been quarantined.
#[test]
#[serial]
fn a_live_worker_writing_into_the_worktree_is_not_reaped() {
    let repo = make_repo(&[(8413, true)]);
    let spec = ProbeSpec {
        uncommitted: true,
        quarantine_ok: true,
        ..ProbeSpec::default()
    };
    // No `with_activity_gate_off` here: `make_repo` just wrote this worktree,
    // so the default 30m window sees it as live — exactly like a worktree with
    // a `cargo` build writing into `target/`.
    let (report, removed, quarantined) = run_pass_full(repo.path(), &spec, &default_opts());
    assert!(removed.is_empty(), "a live worktree must survive the pass");
    assert!(quarantined.is_empty(), "and must not even be stashed out from under the worker");
    assert_eq!(report.skipped.len(), 1);
    assert!(
        report.skipped[0]
            .1
            .contains("live worker with no daemon registry record")
            && report.skipped[0].1.contains("filesystem write"),
        "the skip must name the evidence: {:?}",
        report.skipped
    );
}

/// AC (#8413): the explicit path — a registered in-flight claim fences the
/// worktree off even when it is clean, quiet, and fully reclaimable. This is
/// what an in-session/operator build runs to make itself visible without any
/// daemon sweep registration.
#[test]
#[serial]
fn a_registered_inflight_claim_is_not_reaped() {
    let repo = make_repo(&[(8414, true)]);
    let store = repo.path().join("inflight-store");
    let worktree = repo.path().join(".loom/worktrees/issue-8414");
    std::env::set_var(inflight::INFLIGHT_DIR_ENV, &store);
    claim_tree(&store, "cargo build --workspace", &worktree);

    let spec = ProbeSpec::default();
    let (report, removed) =
        with_activity_gate_off(|| run_pass(repo.path(), &spec, &default_opts()));
    std::env::remove_var(inflight::INFLIGHT_DIR_ENV);

    assert!(removed.is_empty(), "a claimed worktree must survive the pass");
    assert_eq!(report.skipped.len(), 1);
    assert!(
        report.skipped[0].1.contains("in-flight claim")
            && report.skipped[0].1.contains("cargo build --workspace"),
        "the skip must name the claimant: {:?}",
        report.skipped
    );
}

/// The control: with no claim and no recent writes, the same clean, merged
/// worktree is still reaped — the veto must not become "never reclaim".
#[test]
#[serial]
fn a_quiet_unclaimed_worktree_is_still_reaped() {
    let repo = make_repo(&[(8415, true)]);
    let spec = ProbeSpec::default();
    let (report, removed) =
        with_activity_gate_off(|| run_pass(repo.path(), &spec, &default_opts()));
    assert_eq!(removed, vec![8415]);
    assert!(report.skipped.is_empty());
}
