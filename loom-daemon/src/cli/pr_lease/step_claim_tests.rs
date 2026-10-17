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

/// Restores the named env vars on drop.
struct EnvGuard(Vec<(&'static str, Option<String>)>);

impl EnvGuard {
    fn save(names: &[&'static str]) -> Self {
        Self(names.iter().map(|n| (*n, std::env::var(n).ok())).collect())
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// A session in a Codex container has its own hostname. The step child
/// carries the daemon's `LOOM_LEASE_HOST_ID` (`apply_step_env`, forwarded by
/// `spawn-codex.sh`), so the session's published host is the daemon's and
/// it re-attaches to the pre-taken lease instead of reading it as a peer's.
#[test]
#[serial_test::serial]
fn a_containered_session_with_the_daemons_host_id_re_attaches() {
    let _env = EnvGuard::save(&["LOOM_LEASE_HOST_ID", "LOOM_HOST_ID", "HOSTNAME"]);
    std::env::remove_var("LOOM_HOST_ID");
    std::env::set_var("LOOM_LEASE_HOST_ID", "fleet-host-7");
    std::env::set_var("HOSTNAME", "robb-pro.local");
    let daemon_host = published_host();
    std::env::set_var("HOSTNAME", "codex-session-9f2c");
    let session_host = published_host();
    assert_eq!(session_host, daemon_host, "same LOOM_LEASE_HOST_ID, same lease host");
    std::env::remove_var("LOOM_LEASE_HOST_ID");
    assert_ne!(published_host(), daemon_host, "without it, the container is a peer");

    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    let pre = Identity::new(Role::Judge, daemon_host, SWEEP.into(), Some(StepRole::Judge)).unwrap();
    assert_eq!(ctx(dir.path(), pre.clone()).claim(), EXIT_HELD);
    let session = Identity::new(Role::Judge, session_host, SWEEP.into(), None).unwrap();
    assert_eq!(ctx(dir.path(), session).claim(), EXIT_HELD);
    assert_eq!(posted(dir.path()), vec![pre.lease_marker()], "re-attached, never re-posted");
}

/// The step's renewal start is renew-only: it re-attaches to the step's own
/// live lease, and once that lease is released it posts nothing.
#[test]
fn attach_renews_only_a_live_own_lease_and_never_posts() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    let pre = daemon(Role::Doctor, "step-doctor-77-1", StepRole::Doctor);
    assert_eq!(ctx(dir.path(), pre.clone()).attach(), EXIT_EXPIRED, "nothing to renew yet");
    assert!(posted(dir.path()).is_empty(), "attach never posts");
    assert_eq!(ctx(dir.path(), pre.clone()).claim(), EXIT_HELD);
    assert_eq!(ctx(dir.path(), pre.clone()).attach(), EXIT_HELD);
    let peer = daemon(Role::Doctor, "step-doctor-77-2", StepRole::Doctor);
    assert_eq!(ctx(dir.path(), peer).attach(), EXIT_PEER);
    assert_eq!(ctx(dir.path(), pre.clone()).release(None, None), EXIT_HELD);
    assert_eq!(ctx(dir.path(), pre).attach(), EXIT_EXPIRED, "a released lease stays gone");
    assert_eq!(posted(dir.path()).len(), 1, "only the pre-claim ever posted");
}
