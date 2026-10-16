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
    reg.break_fleet_fast_fail_streak(76);
    assert_eq!(reg.peer_fast_failure_streak(76), 0);
}

/// Carry every ad the channel holds into `to`'s view, as the room would,
/// under the sender name `as_host` and the receiving repo's slug (the two test
/// registries have distinct temp roots).
fn relay_as_repo(
    rx: &mut tokio::sync::mpsc::Receiver<ClaimAd>,
    to: &Mutex<PeerClaimView>,
    as_host: &str,
    repo: &str,
) {
    while let Ok(mut ad) = rx.try_recv() {
        ad.host = as_host.into();
        ad.repo = repo.into();
        to.lock()
            .unwrap()
            .observe_dispatch_backoff_at(&ad, Instant::now());
    }
}

/// Host A fast-fails twice, then makes real progress; its break reaches B, so
/// one fast failure on B is the first of a new streak, not the third.
#[test]
fn progress_on_one_host_stops_a_peer_parking_on_stale_counts() {
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();
    let mut a = registry(dir_a.path());
    let (mut b, b_view) = fleet_registry(dir_b.path());
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    a.set_peer_claim_publisher(tx);
    let repo_b = peer_claims::repo_slug(&b.config().workspace_root);

    a.charge_fast_failure("s", 80, "exit-1", false);
    a.charge_fast_failure("t", 80, "exit-1", false);
    relay_as_repo(&mut rx, &b_view, "host-a", &repo_b);
    assert_eq!(b.peer_fast_failure_streak(80), 2);

    // A's dispatch makes real progress.
    a.break_fleet_fast_fail_streak(80);
    relay_as_repo(&mut rx, &b_view, "host-a", &repo_b);
    assert_eq!(b.peer_fast_failure_streak(80), 0);

    b.charge_fast_failure("u", 80, "exit-1", false);
    assert!(!b.is_quarantined(80), "1 failure since the break is not 3 in a row");
}

/// The break clears every peer's entry, not only the sender's: `A:2, C:1` on B
/// is nothing after A's progress.
#[test]
fn a_break_clears_every_hosts_count() {
    let dir = tempdir().unwrap();
    let (b, view) = fleet_registry(dir.path());
    let repo = peer_claims::repo_slug(&b.config().workspace_root);
    peer_backoff(&b, &view, "host-a", 81, 2);
    peer_backoff(&b, &view, "host-c", 81, 1);
    assert_eq!(b.peer_fast_failure_streak(81), 3);
    let brk = ClaimAd::dispatch_backoff_armed(81, repo, "host-a".into(), 1, "ts".into(), 0)
        .as_fast_fail_break();
    view.lock()
        .unwrap()
        .observe_dispatch_backoff_at(&brk, Instant::now());
    assert_eq!(b.peer_fast_failure_streak(81), 0);
}

/// A pre-#11952 sender's break carries no marker: zero window and zero streak
/// is still read as one.
#[test]
fn an_unmarked_legacy_break_still_clears_every_hosts_count() {
    let dir = tempdir().unwrap();
    let (b, view) = fleet_registry(dir.path());
    let repo = peer_claims::repo_slug(&b.config().workspace_root);
    peer_backoff(&b, &view, "host-a", 84, 2);
    peer_backoff(&b, &view, "host-c", 84, 1);
    let mut legacy = ClaimAd::dispatch_backoff_armed(84, repo, "host-a".into(), 1, "ts".into(), 0);
    legacy.consecutive = Some(0);
    assert_eq!(legacy.streak_break, None);
    view.lock()
        .unwrap()
        .observe_dispatch_backoff_at(&legacy, Instant::now());
    assert_eq!(b.peer_fast_failure_streak(84), 0);
}

/// #11952 case 1, the issue's acceptance test: B fast-fails once, A's
/// progress break reaches B, B fails again. B's own count restarts at 1 and
/// its ad says 1, not 2, so C needs three more failures, not one.
#[test]
fn a_break_resets_the_receivers_own_streak() {
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();
    let dir_c = tempdir().unwrap();
    let (mut a, a_view) = fleet_registry(dir_a.path());
    let (mut b, b_view) = fleet_registry(dir_b.path());
    let (mut c, c_view) = fleet_registry(dir_c.path());
    let (tx_a, mut rx_a) = tokio::sync::mpsc::channel(16);
    let (tx_b, mut rx_b) = tokio::sync::mpsc::channel(16);
    a.set_peer_claim_publisher(tx_a);
    b.set_peer_claim_publisher(tx_b);
    let repo_b = peer_claims::repo_slug(&b.config().workspace_root);
    let repo_c = peer_claims::repo_slug(&c.config().workspace_root);

    b.charge_fast_failure("s", 85, "exit-1", false);
    assert_eq!(b.fast_failure_streak(85), 1);
    relay_as_repo(&mut rx_b, &c_view, "host-b", &repo_c);
    assert_eq!(c.peer_fast_failure_streak(85), 1);

    // A has a stale count to break (it saw B's), then makes real progress.
    peer_backoff(&a, &a_view, "host-b", 85, 1);
    a.break_fleet_fast_fail_streak(85);
    let brk = rx_a.try_recv().unwrap();
    assert_eq!(brk.streak_break, Some(true));
    for (view, repo) in [(&b_view, &repo_b), (&c_view, &repo_c)] {
        let mut ad = brk.clone();
        ad.host = "host-a".into();
        ad.repo = repo.clone();
        view.lock()
            .unwrap()
            .observe_dispatch_backoff_at(&ad, Instant::now());
    }
    assert_eq!(b.fast_failure_streak(85), 0, "the break reached B's own count");
    assert_eq!(c.peer_fast_failure_streak(85), 0);

    b.charge_fast_failure("t", 85, "exit-1", false);
    assert_eq!(b.fast_failure_streak(85), 1, "a fresh streak, not 2");
    let ad = rx_b.try_recv().unwrap();
    assert_eq!(ad.consecutive, Some(1), "B advertises 1, not the stale 2");
    relay_as_repo(&mut rx_b, &c_view, "host-b", &repo_c);
    c.charge_fast_failure("u", 85, "exit-1", false);
    assert!(!c.is_quarantined(85), "B:1 + C:1 is two, not three");
    let _ = (&mut a, &a_view);
}

/// #11952: a marked break resets only the fast-fail tally. A window a peer
/// armed for another cause (here, a landed cooldown) keeps running, and so
/// does the receiver's own local window.
#[test]
fn a_break_leaves_every_window_alone() {
    let dir = tempdir().unwrap();
    let (mut b, view) = fleet_registry(dir.path());
    let repo = peer_claims::repo_slug(&b.config().workspace_root);
    let landed =
        ClaimAd::dispatch_backoff_armed(86, repo.clone(), "host-c".into(), 1, "ts".into(), 3600);
    view.lock()
        .unwrap()
        .observe_dispatch_backoff_at(&landed, Instant::now());
    b.charge_fast_failure("s", 86, "exit-1", false);
    let local_before = b.dispatch_backoff_remaining(86, Utc::now());
    assert!(local_before.is_some());

    let brk = ClaimAd::dispatch_backoff_armed(86, repo.clone(), "host-a".into(), 1, "ts".into(), 0)
        .as_fast_fail_break();
    view.lock()
        .unwrap()
        .observe_dispatch_backoff_at(&brk, Instant::now());
    assert!(
        view.lock()
            .unwrap()
            .dispatch_backoff_issues_at(&repo, Instant::now())
            .contains(&86),
        "the peer's landed window survives the break"
    );
    assert!(b.dispatch_backoff_remaining(86, Utc::now()).is_some(), "so does B's own window");
    assert_eq!(b.fast_failure_streak(86), 0, "only the streak is reset");
}

/// #11952: a break only resets a fast-failure record from before it. A
/// failure recorded after the break is a new streak and counts.
#[test]
fn a_failure_after_the_break_still_counts() {
    let dir = tempdir().unwrap();
    let (mut b, view) = fleet_registry(dir.path());
    let repo = peer_claims::repo_slug(&b.config().workspace_root);
    let brk = ClaimAd::dispatch_backoff_armed(87, repo, "host-a".into(), 1, "ts".into(), 0)
        .as_fast_fail_break();
    view.lock()
        .unwrap()
        .observe_dispatch_backoff_at(&brk, Instant::now());
    b.charge_fast_failure("s", 87, "exit-1", false);
    b.charge_fast_failure("t", 87, "exit-1", false);
    assert_eq!(b.fast_failure_streak(87), 2);
    b.charge_fast_failure("u", 87, "exit-1", false);
    assert!(b.is_quarantined(87), "three in a row after the break still park");
}

/// #11952 case 2, the base=0 forgery: a zero base is clamped, and an
/// ordinary transient failure's ad is never a zero window with a zero streak,
/// so neither a new peer nor an older one reads it as a break of every host's
/// count.
#[test]
fn a_zero_base_never_forges_a_break() {
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();
    let mut a = registry(dir_a.path());
    a.set_dispatch_backoff_config(DispatchBackoffConfig {
        enabled: true,
        base: Duration::ZERO,
        max: Duration::ZERO,
    });
    assert_eq!(a.dispatch_backoff_config().base, Duration::from_secs(1), "clamped");
    assert!(a.dispatch_backoff_config().max >= a.dispatch_backoff_config().base);
    let (b, b_view) = fleet_registry(dir_b.path());
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    a.set_peer_claim_publisher(tx);
    let repo_b = peer_claims::repo_slug(&b.config().workspace_root);
    peer_backoff(&b, &b_view, "host-c", 88, 2);

    a.charge_fast_failure("s", 88, "no-usable-account", false);
    let ad = rx.try_recv().unwrap();
    assert_eq!(ad.consecutive, Some(0));
    assert_eq!(ad.streak_break, Some(false));
    assert!(ad.remaining_secs.unwrap() >= 1, "an older peer would read 0/0 as a break");
    let mut relayed = ad;
    relayed.host = "host-a".into();
    relayed.repo = repo_b;
    b_view
        .lock()
        .unwrap()
        .observe_dispatch_backoff_at(&relayed, Instant::now());
    assert_eq!(b.peer_fast_failure_streak(88), 2, "C's count survives A's transient failure");
}

/// #11952: the publisher itself never sends a zero ladder window, whatever
/// delay it is handed.
#[test]
fn a_zero_delay_ladder_ad_is_advertised_as_one_second() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    reg.set_peer_claim_publisher(tx);
    reg.publish_dispatch_backoff_claim(89, Duration::ZERO, 0);
    let ad = rx.try_recv().unwrap();
    assert_eq!(ad.remaining_secs, Some(1));
    assert!(!peer_claims::ClaimAd::from_body_str(&ad.to_body_json())
        .is_some_and(|back| back.streak_break == Some(true)));
}

/// A zero streak on a positive window (a transient cause) still breaks only
/// the sender's own entry.
#[test]
fn a_transient_zero_breaks_only_the_senders_count() {
    let dir = tempdir().unwrap();
    let (b, view) = fleet_registry(dir.path());
    peer_backoff(&b, &view, "host-a", 82, 2);
    peer_backoff(&b, &view, "host-c", 82, 1);
    peer_backoff(&b, &view, "host-a", 82, 0);
    assert_eq!(b.peer_fast_failure_streak(82), 1);
}

/// An operator's `quarantine clear` resets the tally on the clearing host and
/// its peers, so the cleared issue gets a full three strikes, not one.
#[test]
fn quarantine_clear_resets_the_fleet_streak() {
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();
    let (mut a, a_view) = fleet_registry(dir_a.path());
    let (mut b, b_view) = fleet_registry(dir_b.path());
    let (tx, mut rx_b) = tokio::sync::mpsc::channel(16);
    b.set_peer_claim_publisher(tx);
    let repo_a = peer_claims::repo_slug(&a.config().workspace_root);
    // Park from B:1 + peers A:1, C:1, seen on B.
    peer_backoff(&b, &b_view, "host-a", 83, 1);
    peer_backoff(&b, &b_view, "host-c", 83, 1);
    b.charge_fast_failure("s", 83, "exit-1", false);
    assert!(b.is_quarantined(83));
    // A saw B's streak too.
    peer_backoff(&a, &a_view, "host-b", 83, 1);

    assert!(b.clear_quarantine(83));
    assert_eq!(b.peer_fast_failure_streak(83), 0, "the clearing host forgets the peers");
    relay_as_repo(&mut rx_b, &a_view, "host-b", &repo_a);
    assert_eq!(a.peer_fast_failure_streak(83), 0, "and the peers forget it");

    b.charge_fast_failure("t", 83, "exit-1", false);
    assert!(!b.is_quarantined(83), "the first failure after a clear is one strike");
    let _ = &mut a;
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
