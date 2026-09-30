//! #9548 (H7), Judge #9593: the lease-record reader
//! ([`SweepRegistry::read_lease_comments`]) end to end through its two
//! consumers. An outsider's lease comment must neither win the
//! claim-then-verify-order tie-break nor fence the mid-build watchdog's
//! cleanup; the same comment from a trusted author must do both; and a lease
//! whose author the forge did not report is untrusted.
//!
//! The fence leg drives [`SweepRegistry::freshest_lease_owner`] directly (the
//! read `midbuild_lease_veto` acts on) rather than the whole watchdog pass,
//! whose fixtures do not run on every host.

use crate::sweep_registry::test_support::*;
use crate::sweep_registry::*;
use chrono::{Duration, Utc};
use serial_test::serial;
use tempfile::tempdir;

const OUTSIDER: &str =
    r#""user":{"login":"drive-by","type":"User"},"author_association":"CONTRIBUTOR""#;
const OWNER: &str = r#""user":{"login":"rjwalters","type":"User"},"author_association":"OWNER""#;
const NO_AUTHOR: &str = r#""user":null,"author_association":null"#;

/// One NDJSON lease line as the `--jq` projection prints it. `author` is the
/// raw author fields (no braces), or `""` to let the fixture stamp the
/// fleet's own App on it.
fn lease(id: u64, host: &str, sweep: &str, updated: &str, author: &str) -> String {
    let sep = if author.is_empty() { "" } else { "," };
    format!(
        r#"{{"id":{id},"created_at":"{updated}","updated_at":"{updated}","body":"<!-- loom:lease host={host} sweep={sweep} -->"{sep}{author}}}"#
    )
}

fn own_host(dir: &std::path::Path) -> String {
    SweepRegistry::new(SweepRegistryConfig::new(dir.to_path_buf())).published_host_id()
}

/// The tie-break outcome when a peer lease by `peer_author` (id 1) predates
/// this dispatcher's own fleet-authored lease (id 2).
fn tie_break_against(peer_author: &str) -> LeaseOrderDecision {
    let dir = tempdir().unwrap();
    let now = Utc::now();
    let t = now.to_rfc3339();
    let stdout = [
        lease(1, "peer-host", "sweep-peer", &t, peer_author),
        lease(2, &own_host(dir.path()), "sweep-mine", &t, ""),
    ]
    .join("\n");
    let registry = fixture_registry_with_lease_gh(dir.path(), &stdout, 0);
    registry.resolve_lease_order(9548, "sweep-mine", now)
}

#[test]
#[serial]
fn an_untrusted_earlier_lease_does_not_win_the_tie_break() {
    assert_eq!(tie_break_against(OUTSIDER), LeaseOrderDecision::Proceed);
}

#[test]
#[serial]
fn a_trusted_earlier_lease_wins_the_tie_break() {
    assert_eq!(
        tie_break_against(OWNER),
        LeaseOrderDecision::Yield {
            earliest_host: "peer-host".to_string(),
            earliest_sweep_id: "sweep-peer".to_string(),
        }
    );
}

#[test]
#[serial]
fn an_earlier_lease_with_no_author_does_not_win_the_tie_break() {
    assert_eq!(tie_break_against(NO_AUTHOR), LeaseOrderDecision::Proceed);
}

/// The freshest lease owner `midbuild_lease_veto` would fence on.
fn fence_owner(lines: &[String]) -> LeaseOwnerProbeResult {
    let dir = tempdir().unwrap();
    let registry = fixture_registry_with_lease_gh(dir.path(), &lines.join("\n"), 0);
    registry.freshest_lease_owner(9548)
}

#[test]
#[serial]
fn an_untrusted_fresh_lease_does_not_fence_the_watchdog() {
    let old = (Utc::now() - Duration::hours(2)).to_rfc3339();
    let fresh = Utc::now().to_rfc3339();
    // Alone, an outsider's lease is no lease at all.
    assert!(matches!(
        fence_owner(&[lease(1, "spoof-host", "sweep-spoof", &fresh, OUTSIDER)]),
        LeaseOwnerProbeResult::NotFound
    ));
    // Beside the fleet's own (stale) lease, the outsider's fresher one must
    // not become "the current owner".
    match fence_owner(&[
        lease(1, "fleet-host", "sweep-dead", &old, ""),
        lease(2, "spoof-host", "sweep-spoof", &fresh, OUTSIDER),
    ]) {
        LeaseOwnerProbeResult::Found { host, sweep_id, .. } => {
            assert_eq!((host.as_str(), sweep_id.as_str()), ("fleet-host", "sweep-dead"));
        }
        other => panic!("expected the fleet's own lease, got {other:?}"),
    }
}

#[test]
#[serial]
fn a_trusted_fresh_lease_fences_the_watchdog() {
    let fresh = Utc::now().to_rfc3339();
    match fence_owner(&[lease(1, "owner-host", "sweep-live", &fresh, OWNER)]) {
        LeaseOwnerProbeResult::Found { host, sweep_id, .. } => {
            assert_eq!((host.as_str(), sweep_id.as_str()), ("owner-host", "sweep-live"));
        }
        other => panic!("a trusted lease must be seen, got {other:?}"),
    }
}

#[test]
#[serial]
fn a_lease_with_no_author_does_not_fence_the_watchdog() {
    let fresh = Utc::now().to_rfc3339();
    assert!(matches!(
        fence_owner(&[lease(1, "anon-host", "sweep-anon", &fresh, NO_AUTHOR)]),
        LeaseOwnerProbeResult::NotFound
    ));
}
