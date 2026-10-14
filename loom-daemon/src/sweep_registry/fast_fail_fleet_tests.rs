//! Issue #11930: the fast-fail park counted fleet-wide, and a park that
//! survives a daemon restart until its TTL.

use super::*;
use crate::quarantine_reconciliation::{decide_at_startup, BlockedIssue, StartupAction};
use crate::sweep_registry::test_support::backoff_registry;
use tempfile::tempdir;

fn registry(dir: &Path) -> SweepRegistry {
    backoff_registry(dir, DEFAULT_DISPATCH_BACKOFF_BASE_SECS, DEFAULT_DISPATCH_BACKOFF_MAX_SECS)
}

/// A registry wired to a peer view, as on a safehouse-enabled fleet host.
fn fleet_registry(dir: &Path) -> (SweepRegistry, Arc<Mutex<PeerClaimView>>) {
    let mut reg = registry(dir);
    let view = Arc::new(Mutex::new(PeerClaimView::new("self".into(), Duration::from_secs(120))));
    reg.set_peer_claims(Arc::clone(&view));
    (reg, view)
}

/// A peer host's ladder ad, as it arrives over the room.
fn peer_backoff(reg: &SweepRegistry, view: &Mutex<PeerClaimView>, host: &str, issue: u32, n: u32) {
    let repo = peer_claims::repo_slug(&reg.config().workspace_root);
    let ad = ClaimAd::dispatch_backoff_armed(issue, repo, host.into(), 1, "ts".into(), 120)
        .with_fast_fail_streak(n);
    view.lock()
        .unwrap()
        .observe_dispatch_backoff_at(&ad, Instant::now());
}

/// The acceptance case: one genuine fast failure on each of three different
/// hosts parks the issue on the third.
#[test]
fn three_fast_failures_on_three_hosts_park() {
    let dir = tempdir().unwrap();
    let (mut reg, view) = fleet_registry(dir.path());
    peer_backoff(&reg, &view, "host-a", 71, 1);
    peer_backoff(&reg, &view, "host-b", 71, 1);
    assert_eq!(reg.peer_fast_failure_streak(71), 2);
    reg.charge_fast_failure("s", 71, "exit-1", false);
    assert_eq!(reg.fast_failure_streak(71), 1, "only one failure was local");
    assert!(reg.is_quarantined(71), "1 local + 2 peer = the threshold");
}

/// Two hosts' worth is not enough: the threshold is still three.
#[test]
fn two_fast_failures_across_the_fleet_do_not_park() {
    let dir = tempdir().unwrap();
    let (mut reg, view) = fleet_registry(dir.path());
    peer_backoff(&reg, &view, "host-a", 72, 1);
    reg.charge_fast_failure("s", 72, "exit-1", false);
    assert!(!reg.is_quarantined(72));
}

/// The exclusions hold fleet-wide: a peer whose streak was broken by a
/// transient cause advertises 0, and an environmental class here never parks
/// however many peers have counted.
#[test]
fn environmental_and_broken_streaks_never_park_across_the_fleet() {
    let dir = tempdir().unwrap();
    let (mut reg, view) = fleet_registry(dir.path());
    peer_backoff(&reg, &view, "host-a", 73, 2);
    peer_backoff(&reg, &view, "host-b", 73, 2);
    reg.charge_fast_failure("s", 73, "account-exhausted:rate-limited", false);
    reg.charge_fast_failure("t", 73, "preflight-no-cli-start", false);
    assert!(!reg.is_quarantined(73), "the environment's fault never parks");
    // Both peers' streaks are then broken by a transient cause.
    peer_backoff(&reg, &view, "host-a", 73, 0);
    peer_backoff(&reg, &view, "host-b", 73, 0);
    reg.charge_fast_failure("u", 73, "exit-1", false);
    assert!(!reg.is_quarantined(73));
    // And the environmental window stays capped at 900 s.
    reg.charge_fast_failure("v", 74, "execution-error", false);
    assert!(reg.dispatch_backoff_remaining(74, Utc::now()).unwrap() <= Duration::from_secs(900));
}

/// The ladder's ad carries this host's streak, and 0 under any other cause.
#[test]
fn the_backoff_ad_carries_the_local_fast_failure_streak() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    reg.set_peer_claim_publisher(tx);
    reg.charge_fast_failure("s", 75, "exit-1", false);
    reg.charge_fast_failure("t", 75, "exit-1", false);
    let first = rx.try_recv().unwrap();
    let second = rx.try_recv().unwrap();
    assert_eq!(first.kind, peer_claims::ClaimKind::DispatchBackoffArmed);
    assert_eq!((first.consecutive, second.consecutive), (Some(1), Some(2)));
    reg.charge_fast_failure("u", 75, "no-usable-account", false);
    assert_eq!(rx.try_recv().unwrap().consecutive, Some(0), "an environmental class breaks it");
    reg.record_open_pr_guard_backoff(75);
    assert_eq!(rx.try_recv().unwrap().consecutive, Some(0));
}

/// Real progress here forgets the peers' streaks too.
#[test]
fn progress_clears_the_fleet_streak() {
    let dir = tempdir().unwrap();
    let (reg, view) = fleet_registry(dir.path());
    peer_backoff(&reg, &view, "host-a", 76, 2);
    reg.clear_peer_fast_fail_streaks(76);
    assert_eq!(reg.peer_fast_failure_streak(76), 0);
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
