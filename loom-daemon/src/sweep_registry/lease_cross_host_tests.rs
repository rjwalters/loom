//! Issue #11112 (slice 1): what the forge lease ALONE does, and does not,
//! guarantee across two hosts.
//!
//! Loom is dropping its Matrix (safehouse) peer-claim side channel. That
//! channel was only a soft pre-flip advisory; the cross-host claim has always
//! been the forge `loom:lease` comment plus the claim-then-verify-order
//! tie-break ([`SweepRegistry::resolve_lease_order`]). This file pins the
//! behaviour of that tie-break with nothing else in play:
//!
//! - each "host" is its own [`SweepRegistry`] in its own temp workspace, so
//!   the two share **no** local state (no claim lock, no journal);
//! - each runs under its own `LOOM_HOST_ID`, so it recognises only its own
//!   lease record, exactly like two machines;
//! - neither has a peer-claim publisher (safehouse is off by default), so the
//!   only thing the two can both see is the forge's `loom:lease` comment list.
//!
//! **What is verified here is a narrower property than "two hosts never build
//! one issue at once".** When both hosts' leases are visible to each host
//! inside its bounded read-back/confirmation window, exactly one proceeds —
//! the one whose lease the forge numbered first. The tie-break is
//! deliberately FAIL-OPEN, so two hosts CAN both proceed when:
//!
//! - a peer's lease stays invisible past the bounded window (the residual
//!   race — `staggered_visibility_both_proceed_inside_the_invisible_window`);
//! - every lease read-back fails
//!   (`exhausted_read_back_lets_both_hosts_proceed`);
//! - a host never sees its own lease
//!   (`own_lease_never_visible_lets_both_hosts_proceed`).
//!
//! Those three tests assert the limitation on purpose. They are not a bug to
//! fix by flipping an assertion; changing the fail-open policy is a separate
//! decision (see ADR-0025, "Retained fail-open limitation"). Removing the
//! Matrix channel does not change any of this: it never took part in the
//! tie-break.

use crate::sweep_registry::test_support::*;
use crate::sweep_registry::*;
use chrono::{DateTime, Utc};
use serial_test::serial;
use tempfile::tempdir;

const ISSUE: u32 = 11112;

/// Restores `LOOM_HOST_ID` (and the raw-hostname opt-in) on drop, so a
/// failing assertion cannot leak one fake host's identity into later tests.
struct HostIdGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl HostIdGuard {
    fn set(host: &str) -> Self {
        let saved = [HOST_ID_ENV, LEASE_PUBLISH_HOSTNAME_ENV]
            .into_iter()
            .map(|k| (k, std::env::var_os(k)))
            .collect();
        std::env::set_var(HOST_ID_ENV, host);
        std::env::remove_var(LEASE_PUBLISH_HOSTNAME_ENV);
        Self(saved)
    }
}

impl Drop for HostIdGuard {
    fn drop(&mut self) {
        for (k, v) in &self.0 {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

const A: (&str, &str) = ("fleet-host-a", "sweep-a");
const B: (&str, &str) = ("fleet-host-b", "sweep-b");

/// One lease comment as the forge would list it (fleet-authored, so trusted).
fn lease(id: u64, host: &str, sweep: &str, at: &str) -> String {
    format!(
        r#"{{"id":{id},"created_at":"{at}","updated_at":"{at}","body":"<!-- loom:lease host={} sweep={sweep} -->"}}"#,
        opaque_host_id(host)
    )
}

/// What `host` decides for its own `sweep` when every read of the forge's
/// lease comments returns `forge` with exit code `exit` (non-zero = the read
/// fails). The snapshot is fixed for the whole read-back and confirmation
/// window, so it models what this host can SEE, not what has been written.
fn decide_seeing(
    host: &str,
    sweep: &str,
    forge: &str,
    exit: i32,
    episode_start: DateTime<Utc>,
) -> LeaseOrderDecision {
    let _id = HostIdGuard::set(host);
    let workspace = tempdir().unwrap();
    let registry = fixture_registry_with_lease_gh(workspace.path(), forge, exit);
    assert!(
        registry.peer_claim_publisher.is_none(),
        "the proof must not lean on the safehouse peer-claim channel"
    );
    assert_eq!(registry.published_host_id(), opaque_host_id(host));
    registry.resolve_lease_order(ISSUE, sweep, episode_start)
}

fn yield_to(winner: (&str, &str)) -> LeaseOrderDecision {
    LeaseOrderDecision::Yield {
        earliest_host: opaque_host_id(winner.0),
        earliest_sweep_id: winner.1.to_string(),
    }
}

/// Both hosts resolve against the SAME complete snapshot, in which the forge
/// numbered `first`'s lease before `second`'s. Returns each host's decision.
fn resolve_on_one_complete_snapshot(
    first: (&str, &str),
    second: (&str, &str),
) -> (LeaseOrderDecision, LeaseOrderDecision) {
    let now = Utc::now();
    let at = now.to_rfc3339();
    let forge = [
        lease(101, first.0, first.1, &at),
        lease(102, second.0, second.1, &at),
    ]
    .join("\n");
    (
        decide_seeing(first.0, first.1, &forge, 0, now),
        decide_seeing(second.0, second.1, &forge, 0, now),
    )
}

/// Happy path only: both leases visible to both hosts. Exactly one proceeds,
/// and flipping the forge order flips the winner — no host is privileged.
#[test]
#[serial]
fn with_both_leases_visible_exactly_one_of_two_hosts_proceeds() {
    let (a_says, b_says) = resolve_on_one_complete_snapshot(A, B);
    assert_eq!(a_says, LeaseOrderDecision::Proceed);
    assert_eq!(b_says, yield_to(A));

    let (b_says, a_says) = resolve_on_one_complete_snapshot(B, A);
    assert_eq!(b_says, LeaseOrderDecision::Proceed);
    assert_eq!(a_says, yield_to(B));
}

/// **Residual race (documented limitation).** A's lease was numbered first
/// (101), but it is not yet visible to B anywhere in B's bounded read-back
/// and confirmation window: B sees only its own lease (102), so B proceeds.
/// A later resolves with both leases visible and is the earliest, so A
/// proceeds too. Both hosts reach `Proceed`.
///
/// Had A's lease become visible to B inside B's window, B would have yielded
/// (asserted last below; the in-window catch-up is also pinned by
/// `guards::tests::resolve_lease_order_confirmation_finds_a_slow_peer_and_yields`).
/// The window is bounded, so this test documents the gap rather than closing it.
#[test]
#[serial]
fn staggered_visibility_both_proceed_inside_the_invisible_window() {
    let now = Utc::now();
    let at = now.to_rfc3339();
    let a_lease = lease(101, A.0, A.1, &at);
    let b_lease = lease(102, B.0, B.1, &at);
    let both = [a_lease, b_lease.clone()].join("\n");

    let b_early = decide_seeing(B.0, B.1, &b_lease, 0, now);
    let a_late = decide_seeing(A.0, A.1, &both, 0, now);
    assert_eq!(
        (b_early, a_late),
        (LeaseOrderDecision::Proceed, LeaseOrderDecision::Proceed),
        "fail-open limitation: a peer lease invisible past the bounded window lets both hosts \
         proceed (ADR-0025)"
    );

    assert_eq!(
        decide_seeing(B.0, B.1, &both, 0, now),
        yield_to(A),
        "once A's earlier lease is visible to B, the later-numbered host yields"
    );
}

/// **Fail-open limitation.** Every lease read-back fails on both hosts
/// (rate limit, outage). `resolve_lease_order` retries a bounded number of
/// times and then proceeds rather than wedge dispatch, so both hosts proceed.
#[test]
#[serial]
fn exhausted_read_back_lets_both_hosts_proceed() {
    let now = Utc::now();
    let at = now.to_rfc3339();
    let forge = [lease(101, A.0, A.1, &at), lease(102, B.0, B.1, &at)].join("\n");
    assert_eq!(
        (decide_seeing(A.0, A.1, &forge, 1, now), decide_seeing(B.0, B.1, &forge, 1, now),),
        (LeaseOrderDecision::Proceed, LeaseOrderDecision::Proceed),
        "fail-open limitation: an exhausted read-back is not evidence of a peer (ADR-0025)"
    );
}

/// **Fail-open limitation.** Each host sees only the OTHER host's lease and
/// never its own (its write failed, or never propagated within the retry
/// budget). With nothing to compare against, each proceeds.
#[test]
#[serial]
fn own_lease_never_visible_lets_both_hosts_proceed() {
    let now = Utc::now();
    let at = now.to_rfc3339();
    assert_eq!(
        (
            decide_seeing(A.0, A.1, &lease(102, B.0, B.1, &at), 0, now),
            decide_seeing(B.0, B.1, &lease(101, A.0, A.1, &at), 0, now),
        ),
        (LeaseOrderDecision::Proceed, LeaseOrderDecision::Proceed),
        "fail-open limitation: an unverifiable own lease resolves to Proceed (ADR-0025)"
    );
}

// --- Dispatch level: which host actually reaches builder spawn ------------

/// Point `ws`'s harness `gh` at `shared_store` instead of its own lease
/// comment store, so two hosts write to and read from one forge comment list.
fn share_comment_store(
    ws: &std::path::Path,
    own_store: &std::path::Path,
    shared_store: &std::path::Path,
) {
    let fake_gh = ws.join("fake-gh.sh");
    let script = std::fs::read_to_string(&fake_gh).unwrap();
    let own = own_store.display().to_string();
    assert!(script.contains(&own), "harness comment-store arm changed shape");
    std::fs::write(&fake_gh, script.replace(&own, &shared_store.display().to_string())).unwrap();
}

/// Dispatch `ISSUE` on `host`'s registry; returns whether a builder spawned.
fn dispatch_spawns(host: &str, registry: &mut SweepRegistry, spawn_log: &std::path::Path) -> bool {
    let _id = HostIdGuard::set(host);
    assert!(registry.peer_claim_publisher.is_none());
    match registry.dispatch(&SweepKind::Issue(ISSUE), None, None, None, None) {
        Ok(_) => wait_for_contents(spawn_log, "spawned", FIXTURE_CHILD_WAIT_MS),
        Err(err) => {
            assert!(
                err.downcast_ref::<LeaseOrderDispatchError>().is_some(),
                "only a lease-order yield may refuse this dispatch, got: {err:#}"
            );
            assert!(!spawn_log.exists(), "a yield must land before any builder spawn");
            false
        }
    }
}

/// The full dispatch path on one shared forge comment list: A dispatches
/// first, then B. B's read-back sees A's earlier lease, so only A spawns a
/// builder. Sequential, so B's window always includes A's lease — this is the
/// visible case, not a proof of exclusion under concurrent publication.
#[test]
#[serial]
fn dispatch_on_one_visible_forge_spawns_only_the_first_leased_host() {
    let (dir_a, dir_b) = (tempdir().unwrap(), tempdir().unwrap());
    let (mut reg_a, _, spawn_a, store) = lease_order_dispatch_registry(dir_a.path(), &[]);
    let (mut reg_b, _, spawn_b, store_b) = lease_order_dispatch_registry(dir_b.path(), &[]);
    share_comment_store(dir_b.path(), &store_b, &store);

    assert!(dispatch_spawns(A.0, &mut reg_a, &spawn_a), "first leased host must spawn");
    assert!(
        !dispatch_spawns(B.0, &mut reg_b, &spawn_b),
        "later host must yield before spawn"
    );
}

/// **Residual race at dispatch level (documented limitation).** Same two
/// hosts, but each host's forge reads never show the other's lease (separate
/// comment lists = a peer invisible for the whole window). Both spawn.
#[test]
#[serial]
fn dispatch_with_a_peer_lease_never_visible_spawns_on_both_hosts() {
    let (dir_a, dir_b) = (tempdir().unwrap(), tempdir().unwrap());
    let (mut reg_a, _, spawn_a, _) = lease_order_dispatch_registry(dir_a.path(), &[]);
    let (mut reg_b, _, spawn_b, _) = lease_order_dispatch_registry(dir_b.path(), &[]);

    assert!(dispatch_spawns(A.0, &mut reg_a, &spawn_a));
    assert!(
        dispatch_spawns(B.0, &mut reg_b, &spawn_b),
        "fail-open limitation: an invisible peer lease does not stop the second spawn (ADR-0025)"
    );
}
