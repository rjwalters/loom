//! Issue #11930: a fast-fail park that survives a daemon restart until its TTL.

use super::*;
use crate::quarantine_reconciliation::{decide_at_startup, BlockedIssue, StartupAction};
use crate::sweep_registry::test_support::backoff_registry;
use tempfile::tempdir;

fn registry(dir: &Path) -> SweepRegistry {
    backoff_registry(dir, DEFAULT_DISPATCH_BACKOFF_BASE_SECS, DEFAULT_DISPATCH_BACKOFF_MAX_SECS)
}

/// The forge's view of a park `reg` just set, as the startup pass reads it.
fn forge_view(issue: u32, at: DateTime<Utc>) -> BlockedIssue {
    BlockedIssue {
        number: issue,
        has_quarantine_comment: true,
        last_quarantine_comment_at: Some(at),
        last_blocked_labeled_at: Some(at),
    }
}

/// The acceptance case: a park survives a daemon restart until its TTL. The
/// restarted daemon keeps it on the forge and rebuilds the in-memory pause
/// from the marker's timestamp.
#[test]
fn a_park_survives_a_simulated_restart() {
    let dir = tempdir().unwrap();
    let mut before = registry(dir.path());
    for sweep in ["a", "b", "c"] {
        before.charge_fast_failure(sweep, 77, "exit-1", false);
    }
    assert!(before.is_quarantined(77));
    let parked_at = before.quarantine_entries(Utc::now())[0].quarantined_at;
    let ttl = before.quarantine_config.ttl;
    drop(before);

    // The restart: a fresh registry with empty memory, and the startup pass.
    let mut after = registry(dir.path());
    assert!(!after.is_quarantined(77));
    let action = decide_at_startup(&forge_view(77, parked_at), Utc::now(), ttl);
    let StartupAction::Restore { quarantined_at } = action else {
        panic!("a park inside its TTL must not be released: {action:?}");
    };
    crate::quarantine_reconciliation::restored::stash(dir.path(), 77, quarantined_at);

    assert_eq!(after.adopt_restored_parks(), 1);
    after.expire_quarantine();
    assert!(after.is_quarantined(77), "held until its TTL");
    assert!(after.quarantined_issues().contains(&77));
    let left = after.quarantine_entries(Utc::now())[0].ttl_remaining_secs;
    assert!(left > ttl.as_secs() - 60 && left <= ttl.as_secs(), "{left}");
}

/// A park parked by another host is held the same way: the decision reads
/// only the forge, never whether this host set it.
#[test]
fn a_peers_fresh_park_is_held_by_a_restarting_host() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    let at = Utc::now() - chrono::Duration::minutes(10);
    let ttl = reg.quarantine_config.ttl;
    let StartupAction::Restore { quarantined_at } =
        decide_at_startup(&forge_view(78, at), Utc::now(), ttl)
    else {
        panic!("a peer's fresh park must be held");
    };
    crate::quarantine_reconciliation::restored::stash(dir.path(), 78, quarantined_at);
    reg.adopt_restored_parks();
    reg.expire_quarantine();
    assert!(reg.is_quarantined(78));
}

/// The TTL still releases: a park past its TTL at startup is released there,
/// and a restored park whose TTL runs out is released by the ordinary expiry.
#[test]
fn a_ttl_expired_park_is_still_released() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    let ttl = reg.quarantine_config.ttl;
    let old = Utc::now() - chrono::Duration::from_std(ttl).unwrap() - chrono::Duration::seconds(1);
    assert_eq!(decide_at_startup(&forge_view(79, old), Utc::now(), ttl), StartupAction::Release);

    // Adopted just inside its TTL, then the TTL runs out.
    crate::quarantine_reconciliation::restored::stash(dir.path(), 79, old);
    reg.adopt_restored_parks();
    assert!(reg.is_quarantined(79), "adopted");
    reg.expire_quarantine();
    assert!(!reg.is_quarantined(79), "the ordinary TTL expiry releases it");
}

/// An issue already quarantined here keeps its own, newer entry.
#[test]
fn adopting_never_overwrites_a_live_entry() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    reg.enter_quarantine(80);
    let old = Utc::now() - chrono::Duration::days(1);
    crate::quarantine_reconciliation::restored::stash(dir.path(), 80, old);
    assert_eq!(reg.adopt_restored_parks(), 0);
    reg.expire_quarantine();
    assert!(reg.is_quarantined(80));
}
