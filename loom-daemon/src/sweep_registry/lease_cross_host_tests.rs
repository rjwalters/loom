//! Issue #11112 (slice 1): the forge lease ALONE keeps two hosts off the same
//! issue.
//!
//! Loom is dropping its Matrix (safehouse) peer-claim side channel. The
//! property that channel used to soften — two daemons on a shared backlog must
//! never both build one issue — has to hold on the forge record by itself.
//! This file pins that, with nothing else in play:
//!
//! - each "host" is its own [`SweepRegistry`] in its own temp workspace, so
//!   the two share **no** local state (no claim lock, no journal);
//! - each runs under its own `LOOM_HOST_ID`, so it recognises only its own
//!   lease record, exactly like two machines;
//! - neither has a peer-claim publisher (safehouse is off by default), so the
//!   only thing the two can both see is the forge's `loom:lease` comment list.
//!
//! Given that one shared list, the claim-then-verify-order tie-break
//! ([`SweepRegistry::resolve_lease_order`]) must let exactly one host proceed
//! — the one whose lease the forge numbered first — whichever host that is.

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

/// One lease comment as the forge would list it (fleet-authored, so trusted).
fn lease(id: u64, host: &str, sweep: &str, at: &str) -> String {
    format!(
        r#"{{"id":{id},"created_at":"{at}","updated_at":"{at}","body":"<!-- loom:lease host={} sweep={sweep} -->"}}"#,
        opaque_host_id(host)
    )
}

/// What `host` decides for its own `sweep` when the forge lists `forge`.
fn decide(
    host: &str,
    sweep: &str,
    forge: &str,
    episode_start: DateTime<Utc>,
) -> LeaseOrderDecision {
    let _id = HostIdGuard::set(host);
    let workspace = tempdir().unwrap();
    let registry = fixture_registry_with_lease_gh(workspace.path(), forge, 0);
    assert!(
        registry.peer_claim_publisher.is_none(),
        "the proof must not lean on the safehouse peer-claim channel"
    );
    assert_eq!(registry.published_host_id(), opaque_host_id(host));
    registry.resolve_lease_order(ISSUE, sweep, episode_start)
}

/// Both hosts race for `ISSUE`; the forge numbered `first`'s lease before
/// `second`'s. Returns each host's decision.
fn race(first: (&str, &str), second: (&str, &str)) -> (LeaseOrderDecision, LeaseOrderDecision) {
    let now = Utc::now();
    let at = now.to_rfc3339();
    let forge = [
        lease(101, first.0, first.1, &at),
        lease(102, second.0, second.1, &at),
    ]
    .join("\n");
    (decide(first.0, first.1, &forge, now), decide(second.0, second.1, &forge, now))
}

#[test]
#[serial]
fn the_forge_lease_alone_lets_exactly_one_of_two_hosts_build() {
    let a = ("fleet-host-a", "sweep-a");
    let b = ("fleet-host-b", "sweep-b");

    // A's lease landed first: A builds, B yields to A.
    let (a_says, b_says) = race(a, b);
    assert_eq!(a_says, LeaseOrderDecision::Proceed);
    assert_eq!(
        b_says,
        LeaseOrderDecision::Yield {
            earliest_host: opaque_host_id(a.0),
            earliest_sweep_id: a.1.to_string(),
        }
    );

    // Same two hosts, forge order flipped: the winner flips with it. No host
    // is privileged; only the forge-assigned order decides.
    let (b_says, a_says) = race(b, a);
    assert_eq!(b_says, LeaseOrderDecision::Proceed);
    assert_eq!(
        a_says,
        LeaseOrderDecision::Yield {
            earliest_host: opaque_host_id(b.0),
            earliest_sweep_id: b.1.to_string(),
        }
    );
}
