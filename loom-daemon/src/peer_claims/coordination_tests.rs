//! [`super::PeerClaimView::evaluate_coordination`] coverage (Issue #6157, idle
//! gate #8026), kept out of `peer_claims.rs` because that file is frozen at its
//! current size by `scripts/file-size-baseline.txt`, whose own preferred remedy
//! is a sibling module.
//!
//! Issue #8276 raised `DEFAULT_COORDINATION_DEGRADE_GRACE` from 600s to 1200s.
//! That value is a pragmatic compromise, not one derived from a clean
//! measurement (see the constant's doc comment for the corrected derivation) —
//! the grace-boundary and healthy/degrades-at-arbitrary-grace behavior below
//! already exercises the transition generically at any grace value, including
//! this one, so no #8276-specific literal test is added here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::tests::ad;
use super::*;

/// The reaper's default re-advertisement/evaluation cadence
/// (`DEFAULT_REAPER_INTERVAL_SECS`), in seconds.
const REAPER_TICK: u64 = 30;
const RECOVERY: u64 = 3;

/// Replay `to_secs - from_secs` seconds of reaper ticks against `view`, in the
/// same order the real reaper runs them: re-advertise every live claim (#4431),
/// *then* evaluate coordination health (#6157).
///
/// `live = true` models a host with at least one `Running`/`Pending` sweep (so
/// `readvertise_peer_claims` has something to publish); `live = false` models an
/// idle host, which by construction publishes nothing that tick.
fn run_ticks(
    view: &mut PeerClaimView,
    base: Instant,
    from_secs: u64,
    to_secs: u64,
    live: bool,
    grace: Duration,
) -> Vec<CoordinationEvaluation> {
    let mut out = Vec::new();
    let mut t = from_secs;
    while t <= to_secs {
        let at = base + Duration::from_secs(t);
        if live {
            view.record_advertised_at(at);
        }
        out.push(view.evaluate_coordination(at, grace, RECOVERY));
        t += REAPER_TICK;
    }
    out
}

#[test]
fn coordination_stays_healthy_when_never_advertised() {
    let mut view = PeerClaimView::new("me".into(), Duration::from_secs(100));
    let now = Instant::now();
    let eval = view.evaluate_coordination(now, Duration::from_secs(600), 3);
    assert!(!eval.degraded);
    assert!(!eval.transitioned);
    assert!(!view.coordination_degraded());
}

#[test]
fn coordination_stays_healthy_within_grace_with_no_receive_yet() {
    let mut view = PeerClaimView::new("me".into(), Duration::from_secs(1000));
    let base = Instant::now();
    view.record_advertised_at(base);

    let grace = Duration::from_secs(600);
    // Just under the grace window, still advertising on the reaper cadence:
    // still healthy.
    view.record_advertised_at(base + Duration::from_secs(599));
    let eval = view.evaluate_coordination(base + Duration::from_secs(599), grace, 3);
    assert!(!eval.degraded);
    assert!(!eval.transitioned);
}

/// The 2026-08-13 incident's exact signature: sustained advertising
/// (2510 advertised, one per dispatch/reaper heartbeat), zero receives,
/// for hours. Once the grace window elapses with no receive at all,
/// coordination must flip DEGRADED.
///
/// The #8026 idle gate deliberately leaves this path byte-for-byte unchanged —
/// a host advertising continuously into silence is exactly the case the check
/// exists for, and is precisely what the gate's "is this host saying anything?"
/// question answers `true` on.
#[test]
fn coordination_degrades_after_grace_with_zero_receives() {
    let mut view = PeerClaimView::new("robb-studio".into(), Duration::from_secs(1000));
    let base = Instant::now();
    let grace = Duration::from_secs(600);

    // Ten minutes of live-sweep reaper heartbeats, no inbound peer traffic.
    let evals = run_ticks(&mut view, base, 0, 600, true, grace);
    let last = evals.last().expect("at least one tick");
    assert!(last.degraded);
    assert!(last.transitioned, "the tick that crosses the grace window must transition");
    assert_eq!(
        evals.iter().filter(|e| e.transitioned).count(),
        1,
        "exactly one transition — earlier ticks are inside the grace window"
    );
    assert!(view.coordination_degraded());
    assert_eq!(view.coordination_degraded_for_secs(base + Duration::from_secs(600)), Some(0));

    // A later tick, still no receive: still degraded, but no LONGER a
    // transition (already-degraded ticks should not re-fire an alert).
    view.record_advertised_at(base + Duration::from_secs(700));
    let eval2 = view.evaluate_coordination(base + Duration::from_secs(700), grace, 3);
    assert!(eval2.degraded);
    assert!(!eval2.transitioned);
    assert_eq!(view.coordination_degraded_for_secs(base + Duration::from_secs(700)), Some(100));
}

/// A receive that arrives just before the grace window elapses resets
/// the "quiet for" anchor — the receive path is not actually dead, it
/// was just slow once.
#[test]
fn coordination_receive_before_grace_elapses_prevents_degrade() {
    let mut view = PeerClaimView::new("me".into(), Duration::from_secs(1000));
    let base = Instant::now();
    view.record_advertised_at(base);

    let grace = Duration::from_secs(600);
    // A genuine peer receive lands at t+590, just inside the window.
    view.observe_at(&ad(ClaimKind::Advertise, 1, "loom", "peer"), base + Duration::from_secs(590));

    // At t+600 (which would have tripped the grace measured from
    // first-advertised) coordination is still healthy: the anchor moved
    // to the receive at t+590.
    view.record_advertised_at(base + Duration::from_secs(600));
    let eval = view.evaluate_coordination(base + Duration::from_secs(600), grace, 3);
    assert!(!eval.degraded);
    assert!(!eval.transitioned);
}

/// Issue #6157 AC4: recovery requires SUSTAINED receives, not a single
/// one — a lone stray ad must not immediately clear a DEGRADED verdict.
#[test]
fn coordination_recovery_requires_sustained_not_single_receive() {
    let mut view = PeerClaimView::new("me".into(), Duration::from_secs(1000));
    let base = Instant::now();
    let grace = Duration::from_secs(600);
    let recovery_threshold = 3;

    // Trip DEGRADED.
    view.record_advertised_at(base);
    view.record_advertised_at(base + Duration::from_secs(600));
    let eval =
        view.evaluate_coordination(base + Duration::from_secs(600), grace, recovery_threshold);
    assert!(eval.degraded && eval.transitioned);

    // A single receive lands while degraded: not enough to recover.
    view.observe_at(&ad(ClaimKind::Advertise, 1, "loom", "peer"), base + Duration::from_secs(610));
    let eval2 =
        view.evaluate_coordination(base + Duration::from_secs(620), grace, recovery_threshold);
    assert!(eval2.degraded, "a single receive must not clear a DEGRADED verdict");
    assert!(!eval2.transitioned);
    assert_eq!(view.coordination_receives_toward_recovery(), 1);

    // Two more receives land — three consecutive total, meeting the
    // threshold.
    view.observe_at(&ad(ClaimKind::Retract, 1, "loom", "peer"), base + Duration::from_secs(630));
    view.observe_at(&ad(ClaimKind::Advertise, 2, "loom", "peer"), base + Duration::from_secs(640));
    let eval3 =
        view.evaluate_coordination(base + Duration::from_secs(650), grace, recovery_threshold);
    assert!(!eval3.degraded, "3 consecutive receives must clear the DEGRADED verdict");
    assert!(eval3.transitioned);
    assert!(!view.coordination_degraded());
    assert_eq!(view.coordination_receives_toward_recovery(), 0);
}

/// A self-advertisement is never counted as a receive (mirrors
/// `own_advertisement_is_never_backed_off_on`), so it can never
/// manufacture a false recovery signal for THIS host's own DEGRADED
/// coordination.
#[test]
fn coordination_recovery_ignores_self_advertisements() {
    let mut view = PeerClaimView::new("me".into(), Duration::from_secs(1000));
    let base = Instant::now();
    let grace = Duration::from_secs(600);
    view.record_advertised_at(base);
    view.record_advertised_at(base + Duration::from_secs(600));
    let eval = view.evaluate_coordination(base + Duration::from_secs(600), grace, 3);
    assert!(eval.degraded && eval.transitioned);

    // Re-advertising (a reaper heartbeat) and our own ad arriving back
    // somehow must not count toward recovery.
    view.record_advertised_at(base + Duration::from_secs(610));
    assert!(!view
        .observe_at(&ad(ClaimKind::Advertise, 1, "loom", "me"), base + Duration::from_secs(611)));
    assert_eq!(view.coordination_receives_toward_recovery(), 0);

    let eval2 = view.evaluate_coordination(base + Duration::from_secs(620), grace, 3);
    assert!(eval2.degraded, "self-ads must never clear a DEGRADED verdict");
}

// ---- the idle gate (Issue #8026) ----

/// **The bug this issue is about.** Advertising is entirely dispatch-gated
/// (`readvertise_peer_claims` only re-advertises `Running`/`Pending` entries),
/// so during a fleet-wide dispatch lull every host transmits nothing, every
/// host therefore receives nothing, and every host's quiet clock runs out at
/// roughly the same moment — a fleet-wide simultaneous false DEGRADED with
/// nothing broken.
///
/// Here: five minutes of real work (live heartbeats, peers active), then a
/// three-hour lull in which this host has no live sweeps. It must stay healthy
/// throughout, however long the lull runs.
#[test]
fn a_fleet_wide_idle_lull_never_degrades_coordination() {
    let mut view = PeerClaimView::new("robb-pro".into(), Duration::from_secs(1000));
    let base = Instant::now();
    let grace = Duration::from_secs(1200);

    // Busy: peers are advertising too, so receives land normally.
    for t in [0_u64, 30, 60, 90, 120] {
        view.record_advertised_at(base + Duration::from_secs(t));
        view.observe_at(
            &ad(ClaimKind::Advertise, 1, "loom", "peer"),
            base + Duration::from_secs(t),
        );
        view.evaluate_coordination(base + Duration::from_secs(t), grace, RECOVERY);
    }
    assert!(!view.coordination_degraded());

    // The lull: nobody anywhere has work in flight, so this host publishes
    // nothing and hears nothing — for 3 hours, 9× the grace window.
    let evals = run_ticks(&mut view, base, 150, 10_800, false, grace);
    assert!(
        evals.iter().all(|e| !e.degraded),
        "a dispatch lull is not evidence of a broken receive path (#8026)"
    );
    assert!(evals.iter().all(|e| !e.transitioned), "no transition ⇒ no watchdog escalation");
    assert!(!view.coordination_degraded());
    assert!(
        evals
            .last()
            .expect("ticks ran")
            .reason
            .contains("not currently advertising"),
        "the verdict must say WHY it was withheld"
    );
}

/// The test plan's edge case: a host that goes idle mid-window and later picks
/// work back up must resume normal evaluation with a **full fresh grace
/// window** — not inherit the stale receive anchor from before the lull and
/// flip DEGRADED on its first tick back.
#[test]
fn resuming_dispatch_after_an_idle_lull_starts_a_fresh_grace_window() {
    let mut view = PeerClaimView::new("robb-studio".into(), Duration::from_secs(1000));
    let base = Instant::now();
    let grace = Duration::from_secs(1200);

    // One dispatch, one receive, then a two-hour lull.
    view.record_advertised_at(base);
    view.observe_at(&ad(ClaimKind::Advertise, 1, "loom", "peer"), base);
    run_ticks(&mut view, base, 30, 7_200, false, grace);
    assert!(!view.coordination_degraded());

    // Work resumes at t+7200. The last genuine receive is now 2h old — far
    // past `grace` — but the clock was rebased through the lull, so the first
    // ticks back are healthy.
    let resumed = run_ticks(&mut view, base, 7_200, 7_200 + 1_170, true, grace);
    assert!(
        resumed.iter().all(|e| !e.degraded),
        "a resumed host must get a full grace window to hear from its peers"
    );

    // Only once a FULL grace window of continuous advertising has gone by with
    // no receive does the genuine-break verdict fire.
    let later = run_ticks(&mut view, base, 7_200 + 1_200, 7_200 + 1_200, true, grace);
    let last = later.last().expect("one tick");
    assert!(last.degraded && last.transitioned);
    assert!(last.reason.contains("no peer claim received"));
}

/// Going idle is not a recovery signal. An already-DEGRADED verdict still
/// clears only on `recovery_threshold` sustained receives (#6157 AC4) — the
/// gate withholds *new* verdicts, it never manufactures a recovery.
#[test]
fn going_idle_does_not_clear_an_already_degraded_verdict() {
    let mut view = PeerClaimView::new("me".into(), Duration::from_secs(1000));
    let base = Instant::now();
    let grace = Duration::from_secs(600);

    let evals = run_ticks(&mut view, base, 0, 600, true, grace);
    assert!(evals.last().expect("ticks ran").degraded);

    // Now go idle for an hour: still degraded, still no transition.
    let idle = run_ticks(&mut view, base, 630, 4_200, false, grace);
    assert!(idle.iter().all(|e| e.degraded && !e.transitioned));
    assert!(view.coordination_degraded());

    // Sustained receives are still the only way out.
    for t in [4_230_u64, 4_260, 4_290] {
        view.observe_at(
            &ad(ClaimKind::Advertise, 1, "loom", "peer"),
            base + Duration::from_secs(t),
        );
    }
    let out = view.evaluate_coordination(base + Duration::from_secs(4_320), grace, RECOVERY);
    assert!(!out.degraded && out.transitioned);
}

/// The window must be several reaper cadences wide, because an evaluation does
/// not always sit immediately behind one of *this* registry's own ads: a daemon
/// managing several repos (#3928) shares one view across registries and runs a
/// `readvertise → evaluate` pair per registry per tick, so a registry whose own
/// repo is idle evaluates against an ad published up to a full tick earlier. A
/// host running a deliberately slow reaper can widen the window so those
/// evaluations stay under judgment instead of reading as idleness.
#[test]
fn a_widened_activity_window_keeps_a_slow_reaper_under_judgment() {
    let grace = Duration::from_secs(1200);
    // A 300s reaper cadence: one tick is already beyond the 180s default.
    let slow_tick = Duration::from_secs(300);
    let base = Instant::now();

    let mut default_window = PeerClaimView::new("slow".into(), Duration::from_secs(1000));
    let mut widened = PeerClaimView::new("slow".into(), Duration::from_secs(1000));
    widened.set_advertise_activity_window(Duration::from_secs(900));

    // Both advertised continuously (zero receives) right up to t+grace...
    for v in [&mut default_window, &mut widened] {
        v.record_advertised_at(base);
        v.record_advertised_at(base + grace);
    }
    // ...and the evaluating tick lands one slow cadence later, with no ad of
    // its own in front of it. The quiet clock is well past `grace` by now.
    let at = base + grace + slow_tick;
    assert!(
        !default_window
            .evaluate_coordination(at, grace, RECOVERY)
            .degraded,
        "at the default 180s window a 300s-cadence host reads as idle between ticks"
    );
    assert!(
        widened.evaluate_coordination(at, grace, RECOVERY).degraded,
        "widening the window restores judgment for a slow reaper"
    );
}

/// The gate reads this host's own outbound advertisements, which are counted
/// for every publish attempt regardless of whether the channel had a live
/// consumer (#5921's fail-open contract) — so a genuinely *broken* transport
/// still opens the gate rather than silencing the check that would report it.
#[test]
fn a_dropped_outbound_ad_still_counts_as_advertising() {
    let mut view = PeerClaimView::new("me".into(), Duration::from_secs(1000));
    let base = Instant::now();
    let grace = Duration::from_secs(600);

    // `record_advertised` is called BEFORE the `try_send` in
    // `publish_peer_claim`, so these model ads the channel then dropped.
    let evals = run_ticks(&mut view, base, 0, 600, true, grace);
    assert!(evals.last().expect("ticks ran").degraded);
}
