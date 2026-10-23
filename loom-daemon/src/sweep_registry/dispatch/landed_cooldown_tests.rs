//! Issue #11853: the landed cooldown.

use super::*;
use crate::peer_claims::ClaimKind;
use crate::sweep_registry::test_support::fixture_registry;
use crate::sweep_registry::{DispatchBackoffError, LANDED_COOLDOWN};
use crate::types::SweepKind;
use serial_test::serial;
use tempfile::tempdir;

/// Armed: hours long (past the 900 s ladder cap), broadcast to peers with
/// its full length, and it refuses the next dispatch without spawning.
#[test]
#[serial]
fn a_landed_cooldown_is_long_fleet_wide_and_suppresses_redispatch() {
    let dir = tempdir().unwrap();
    let (mut registry, spawn_log) = fixture_registry(dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    registry.set_peer_claim_publisher(tx);

    registry.arm_landed_cooldown(11853, LANDED_COOLDOWN);

    let now = Utc::now();
    assert!(registry.landed_cooldown_active(11853));
    assert!(registry.dispatch_backoff_issues(now).contains(&11853));
    let remaining = registry.dispatch_backoff_remaining(11853, now).unwrap();
    assert!(remaining > Duration::from_secs(5 * 3600), "{remaining:?}");
    // #11854: the ladder's `max` is now 6 h too, but every cause except a
    // genuine fast failure stays at 900 s; the landed window is far longer.
    assert!(
        remaining > super::super::backoff_cause::TRANSIENT_BACKOFF_CAP,
        "longer than the #4485 ladder's transient cap"
    );

    let ad = rx.try_recv().expect("the window is broadcast");
    assert_eq!(ad.kind, ClaimKind::DispatchBackoffArmed);
    assert_eq!(ad.issue, 11853);
    assert!(ad.remaining_secs.unwrap() > 5 * 3600, "{:?}", ad.remaining_secs);

    let err = registry
        .dispatch(&SweepKind::Issue(11853), None, None, None, None)
        .expect_err("re-dispatch is suppressed");
    assert!(err.downcast_ref::<DispatchBackoffError>().is_some(), "{err:#}");
    assert!(!spawn_log.exists(), "nothing was spawned");
}

/// A re-arm never shortens a longer window already in force, and a
/// disabled backoff arms nothing.
#[test]
fn a_rearm_only_extends_and_a_disabled_backoff_arms_nothing() {
    let dir = tempdir().unwrap();
    let (mut registry, _) = fixture_registry(dir.path());
    registry.arm_landed_cooldown(7, LANDED_COOLDOWN);
    registry.arm_landed_cooldown(7, Duration::from_secs(60));
    let remaining = registry.dispatch_backoff_remaining(7, Utc::now()).unwrap();
    assert!(remaining > Duration::from_secs(5 * 3600), "{remaining:?}");

    registry.dispatch_backoff_config.enabled = false;
    registry.arm_landed_cooldown(8, LANDED_COOLDOWN);
    assert!(!registry.landed_cooldown_active(8));
}

/// A peer that receives the broadcast skips the issue for the whole window:
/// the receiving lane does not cap it.
#[test]
fn a_peer_honours_the_full_window() {
    let dir = tempdir().unwrap();
    let (mut registry, _) = fixture_registry(dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    registry.set_peer_claim_publisher(tx);
    registry.arm_landed_cooldown(11853, LANDED_COOLDOWN);
    let mut ad = rx.try_recv().unwrap();
    ad.host = "some-peer".to_string();

    let mut view = crate::peer_claims::PeerClaimView::new(
        "this-host".to_string(),
        crate::peer_claims::DEFAULT_PEER_CLAIM_TTL,
    );
    let t0 = std::time::Instant::now();
    assert!(view.observe_dispatch_backoff_at(&ad, t0));
    let later = t0 + Duration::from_secs(5 * 3600);
    assert!(view
        .dispatch_backoff_issues_at(&ad.repo, later)
        .contains(&11853));
}

/// #11901 case 3: a peer keeps the longer window. A later, shorter
/// broadcast for the same issue (a ladder window from another host) does not
/// shorten a landed cooldown; a zero-length one, the lift, clears it.
#[test]
fn a_shorter_peer_broadcast_never_shortens_the_window() {
    let dir = tempdir().unwrap();
    let (mut registry, _) = fixture_registry(dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    registry.set_peer_claim_publisher(tx);
    registry.arm_landed_cooldown(11901, LANDED_COOLDOWN);
    let mut long = rx.try_recv().unwrap();
    long.host = "peer-a".to_string();
    let mut short = long.clone();
    short.host = "peer-b".to_string();
    short.remaining_secs = Some(60);

    let mut view = crate::peer_claims::PeerClaimView::new(
        "this-host".to_string(),
        crate::peer_claims::DEFAULT_PEER_CLAIM_TTL,
    );
    let t0 = std::time::Instant::now();
    assert!(view.observe_dispatch_backoff_at(&long, t0));
    assert!(view.observe_dispatch_backoff_at(&short, t0 + Duration::from_secs(1)));
    let remaining = view
        .dispatch_backoff_remaining_at(&long.repo, 11901, t0)
        .unwrap();
    assert!(remaining > Duration::from_secs(5 * 3600), "{remaining:?}");

    let mut clear = short.clone();
    clear.remaining_secs = Some(0);
    assert!(view.observe_dispatch_backoff_at(&clear, t0));
    assert!(!view
        .dispatch_backoff_issues_at(&long.repo, t0)
        .contains(&11901));
}

/// #11901 case 4: lifting removes this host's landed window (never a
/// failure-ladder one), drops its copy of a peer's window for the issue, and
/// broadcasts a zero-length window, the clear.
#[test]
fn clearing_a_landed_cooldown_lifts_it_here_and_on_peers() {
    use std::sync::{Arc, Mutex};
    let dir = tempdir().unwrap();
    let (mut registry, _) = fixture_registry(dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    registry.set_peer_claim_publisher(tx);
    let view = Arc::new(Mutex::new(crate::peer_claims::PeerClaimView::new(
        "this-host".to_string(),
        crate::peer_claims::DEFAULT_PEER_CLAIM_TTL,
    )));
    registry.set_peer_claims(view.clone());

    registry.arm_landed_cooldown(5, LANDED_COOLDOWN);
    let mut peer_ad = rx.try_recv().unwrap();
    peer_ad.host = "some-peer".to_string();
    view.lock()
        .unwrap()
        .observe_dispatch_backoff_at(&peer_ad, std::time::Instant::now());
    assert!(registry.landed_cooldown_issues().contains(&5));

    assert!(registry.clear_landed_cooldown(5));
    assert!(!registry.landed_cooldown_active(5));
    assert!(!registry.dispatch_backoff_issues(Utc::now()).contains(&5), "the peer copy too");
    let clear = rx.try_recv().expect("the lift is broadcast");
    assert_eq!(clear.kind, ClaimKind::DispatchBackoffArmed);
    assert_eq!((clear.issue, clear.remaining_secs), (5, Some(0)));

    // A failure-ladder window is not a landed cooldown: it stays.
    registry.record_dispatch_failure(6);
    assert!(!registry.landed_cooldown_issues().contains(&6));
    assert!(!registry.clear_landed_cooldown(6));
    assert!(registry.dispatch_backoff_remaining(6, Utc::now()).is_some());
}
