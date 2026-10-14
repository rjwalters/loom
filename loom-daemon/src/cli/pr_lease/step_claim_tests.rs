//! B1 (#11906): the step engine pre-takes a non-Builder step's role-keyed
//! lease (`--step`, under the step's sweep id), and the session it spawns
//! must read that lease as its own. Ownership is role, host and sweep; the
//! step is metadata (#11950), so the prompt's `lease pr` calls, which pass no
//! `--step`, re-attach.

use super::*;

const SWEEP: &str = "step-judge-77-1760000000";

fn daemon(role: Role, sweep: &str, step: StepRole) -> Identity {
    Identity::new(role, "host-a".into(), sweep.into(), Some(step)).unwrap()
}

/// The blocker: the session claims with no `--step`, under the step's sweep
/// id, and must re-attach to the daemon's lease rather than stand down.
#[test]
fn a_step_session_claims_its_daemons_pre_taken_lease_as_its_own() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    let pre = daemon(Role::Judge, SWEEP, StepRole::Judge);
    assert_eq!(ctx(dir.path(), pre.clone()).claim(), EXIT_HELD);

    let session = Identity::new(Role::Judge, "host-a".into(), SWEEP.into(), None).unwrap();
    assert_eq!(ctx(dir.path(), session.clone()).claim(), EXIT_HELD, "the session's own lease");
    assert_eq!(posted(dir.path()), vec![pre.lease_marker()], "re-attached, never re-posted");
    assert_eq!(ctx(dir.path(), session.clone()).check(), EXIT_HELD);
    assert_eq!(ctx(dir.path(), session).fence(), EXIT_HELD);

    // Another sweep is still a peer.
    let other = daemon(Role::Judge, "step-judge-77-1760000099", StepRole::Judge);
    assert_eq!(ctx(dir.path(), other).claim(), EXIT_PEER);
}

/// A timed-out step's lease is reaped by the dead claimant's sweep identity
/// (`lease pr reap` passes no step).
#[test]
fn a_timed_out_steps_lease_is_reaped_by_its_sweep_identity() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    let pre = daemon(Role::Curator, "step-curator-77-1", StepRole::Curator);
    assert_eq!(ctx(dir.path(), pre).claim(), EXIT_HELD);
    let reaper =
        Identity::new(Role::Curator, "host-a".into(), "step-curator-77-1".into(), None).unwrap();
    assert_eq!(ctx(dir.path(), reaper).reap("timeout"), EXIT_HELD);
    assert!(
        patched(dir.path()).contains("loom:pr-lease-released role=curator"),
        "{}",
        patched(dir.path())
    );
    let fresh = daemon(Role::Curator, "step-curator-77-2", StepRole::Curator);
    assert_eq!(ctx(dir.path(), fresh).claim(), EXIT_HELD, "the claim is free again");
}
