//! Issue #11853: the landed cooldown.

use super::*;
use crate::sweep_registry::test_support::fixture_registry;
use crate::sweep_registry::{DispatchBackoffError, LANDED_COOLDOWN};
use crate::types::SweepKind;
use serial_test::serial;
use tempfile::tempdir;

/// Armed: hours long (past the 900 s ladder cap), and it refuses the next dispatch without spawning.
#[test]
#[serial]
fn a_landed_cooldown_is_long_and_suppresses_redispatch() {
    let dir = tempdir().unwrap();
    let (mut registry, spawn_log) = fixture_registry(dir.path());

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

/// #11901 case 4: lifting removes this host's landed window, never a
/// failure-ladder one.
#[test]
fn clearing_a_landed_cooldown_lifts_it_but_not_a_failure_window() {
    let dir = tempdir().unwrap();
    let (mut registry, _) = fixture_registry(dir.path());

    registry.arm_landed_cooldown(5, LANDED_COOLDOWN);
    assert!(registry.landed_cooldown_issues().contains(&5));

    assert!(registry.clear_landed_cooldown(5));
    assert!(!registry.landed_cooldown_active(5));
    assert!(!registry.dispatch_backoff_issues(Utc::now()).contains(&5));

    // A failure-ladder window is not a landed cooldown: it stays.
    registry.record_dispatch_failure(6);
    assert!(!registry.landed_cooldown_issues().contains(&6));
    assert!(!registry.clear_landed_cooldown(6));
    assert!(registry.dispatch_backoff_remaining(6, Utc::now()).is_some());
}
