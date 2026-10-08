use super::*;
use serial_test::serial;

const BASE: Duration = Duration::from_secs(60);
const WS: &str = "/ws/a";
const REPO: &str = "o/a";

fn gate(enabled: bool) -> PollGate<Vec<u32>> {
    PollGate::new(enabled, BASE)
}

/// Poll `WS` once at `now` and record `result`.
fn poll(g: &mut PollGate<Vec<u32>>, reason: PollReason, result: Vec<u32>, now: Instant) {
    let clock = g.clock();
    let fp = u64::from(result.iter().sum::<u32>());
    g.record_poll(WS, REPO, reason, clock, fp, result, now);
}

fn decide(g: &mut PollGate<Vec<u32>>, healthy: bool, now: Instant) -> Decision {
    g.decide(WS, Some(REPO), ReadKind::Discovery, healthy, now)
}

#[test]
fn healthy_with_no_event_skips_and_serves_the_held_listing() {
    let mut g = gate(true);
    let t0 = Instant::now();
    assert_eq!(decide(&mut g, true, t0), Decision::Poll(PollReason::FirstPoll));
    poll(&mut g, PollReason::FirstPoll, vec![1, 2], t0);
    let t1 = t0 + Duration::from_secs(61);
    assert_eq!(decide(&mut g, true, t1), Decision::Skip);
    assert_eq!(g.held(WS), Some(vec![1, 2]));
    assert!(g.snapshot(true, t1).workspaces[0].gated);
    assert_eq!(g.snapshot(true, t1).polls_skipped, 1);
}

#[test]
fn an_event_for_the_repo_forces_a_repoll_but_another_repo_does_not() {
    let mut g = gate(true);
    let t0 = Instant::now();
    poll(&mut g, PollReason::FirstPoll, vec![1], t0);
    g.note_event(Some("o/other"));
    assert_eq!(decide(&mut g, true, t0 + BASE), Decision::Skip);
    g.note_event(Some("O/A")); // case-insensitive
    assert_eq!(decide(&mut g, true, t0 + BASE), Decision::Poll(PollReason::Event));
    poll(&mut g, PollReason::Event, vec![1, 3], t0 + BASE);
    // The event is consumed by the poll that followed it.
    assert_eq!(decide(&mut g, true, t0 + BASE * 2), Decision::Skip);
    assert_eq!(g.snapshot(true, t0).event_repolls, 1);
}

#[test]
fn an_event_landing_mid_poll_is_not_swallowed() {
    let mut g = gate(true);
    let t0 = Instant::now();
    let clock_before_poll = g.clock();
    g.note_event(Some(REPO)); // arrives while the poll is in flight
    g.record_poll(WS, REPO, PollReason::FirstPoll, clock_before_poll, 0, vec![], t0);
    assert_eq!(decide(&mut g, true, t0), Decision::Poll(PollReason::Event));
}

#[test]
fn unattributable_events_and_clamped_pages_invalidate_every_workspace() {
    let mut g = gate(true);
    let t0 = Instant::now();
    poll(&mut g, PollReason::FirstPoll, vec![], t0);
    g.note_event(None);
    assert_eq!(decide(&mut g, true, t0), Decision::Poll(PollReason::Event));
    poll(&mut g, PollReason::Event, vec![], t0);
    g.note_event(Some("not a repo/../x"));
    assert_eq!(decide(&mut g, true, t0), Decision::Poll(PollReason::Event));
    poll(&mut g, PollReason::Event, vec![], t0);
    g.note_unattributable();
    assert_eq!(decide(&mut g, true, t0), Decision::Poll(PollReason::Event));
}

#[test]
fn the_hard_cap_forces_a_repoll_and_is_ten_x_base_up_to_fifteen_minutes() {
    assert_eq!(hard_cap(Duration::from_secs(60)), Duration::from_secs(600));
    assert_eq!(hard_cap(Duration::from_secs(120)), Duration::from_secs(900));
    // A loop already slower than the ceiling is never stretched or shortened.
    assert_eq!(hard_cap(Duration::from_secs(1800)), Duration::from_secs(1800));
    let mut g = gate(true);
    let t0 = Instant::now();
    poll(&mut g, PollReason::FirstPoll, vec![1], t0);
    assert_eq!(decide(&mut g, true, t0 + Duration::from_secs(599)), Decision::Skip);
    assert_eq!(
        decide(&mut g, true, t0 + Duration::from_secs(600)),
        Decision::Poll(PollReason::HardCap)
    );
}

#[test]
fn a_hard_cap_repoll_that_found_a_change_is_counted_lossy() {
    let mut g = gate(true);
    let t0 = Instant::now();
    poll(&mut g, PollReason::FirstPoll, vec![1], t0);
    let t1 = t0 + Duration::from_secs(600);
    // Unchanged: a hard-cap repoll, not a lossy one.
    poll(&mut g, PollReason::HardCap, vec![1], t1);
    // Changed with no event ever reported: lossy.
    poll(&mut g, PollReason::HardCap, vec![1, 9], t1 + Duration::from_secs(600));
    let snap = g.snapshot(true, t1);
    assert_eq!((snap.hard_cap_repolls, snap.lossy_repolls), (2, 1));
    assert_eq!(snap.lossy_rate(), Some(0.5));
    // An event-driven repoll that changed the listing is the feed working.
    poll(&mut g, PollReason::Event, vec![5], t1);
    assert_eq!(g.snapshot(true, t1).lossy_repolls, 1);
}

#[test]
fn any_non_healthy_status_restores_base_cadence_with_no_grace() {
    let mut g = gate(true);
    let t0 = Instant::now();
    poll(&mut g, PollReason::FirstPoll, vec![1], t0);
    assert_eq!(decide(&mut g, true, t0 + BASE), Decision::Skip);
    // The very next read after the feed degrades polls, one second later.
    assert_eq!(
        decide(&mut g, false, t0 + BASE + Duration::from_secs(1)),
        Decision::Poll(PollReason::FeedNotHealthy)
    );
    let snap = g.snapshot(false, t0);
    assert!(!snap.gating_active);
    assert!(!snap.workspaces[0].gated);
}

#[test]
fn reads_that_gate_a_claim_label_transition_or_merge_are_never_held() {
    let mut g = gate(true);
    let t0 = Instant::now();
    poll(&mut g, PollReason::FirstPoll, vec![1], t0);
    for kind in [ReadKind::Claim, ReadKind::LabelTransition, ReadKind::Merge] {
        assert_eq!(
            g.decide(WS, Some(REPO), kind, true, t0),
            Decision::Poll(PollReason::DecisionRead),
            "{kind:?}"
        );
    }
}

#[test]
fn a_workspace_with_no_resolvable_repo_is_never_gated() {
    let mut g = gate(true);
    let t0 = Instant::now();
    poll(&mut g, PollReason::FirstPoll, vec![1], t0);
    assert_eq!(
        g.decide(WS, None, ReadKind::Discovery, true, t0),
        Decision::Poll(PollReason::Ungated)
    );
}

#[test]
fn a_disabled_gate_always_polls_and_records_nothing() {
    let mut g = gate(false);
    let t0 = Instant::now();
    g.note_event(Some(REPO));
    assert_eq!(decide(&mut g, true, t0), Decision::Poll(PollReason::Ungated));
    poll(&mut g, PollReason::Ungated, vec![1], t0);
    assert_eq!(decide(&mut g, true, t0), Decision::Poll(PollReason::Ungated));
    assert_eq!(g.held(WS), None);
    assert_eq!(g.clock(), 0);
}

#[test]
#[serial]
fn pollgating_resolves_env_over_config_over_default_off() {
    std::env::remove_var(POLL_GATING_ENV);
    let mut config = crate::forge_events::ForgeEventsConfig::default();
    assert!(!resolve_poll_gating(&config));
    config.poll_gating = Some(true);
    assert!(resolve_poll_gating(&config));
    std::env::set_var(POLL_GATING_ENV, "0");
    assert!(!resolve_poll_gating(&config));
    std::env::remove_var(POLL_GATING_ENV);
}

/// Default-off is byte-identical: the global gated path just runs the closure
/// every time and exposes no status block.
#[test]
#[serial]
fn the_global_path_is_a_bare_call_when_gating_is_off() {
    configure(false, BASE);
    let mut calls = 0;
    for _ in 0..3 {
        let out: Result<_, ()> = gated_list(None, Some(REPO), || {
            calls += 1;
            Ok(vec![])
        });
        assert!(out.unwrap().is_empty());
    }
    assert_eq!(calls, 3);
    assert!(snapshot().is_none());
}

#[test]
#[serial]
fn ingest_page_is_a_noop_when_off_and_records_when_on() {
    configure(false, BASE);
    ingest_page(&[serde_json::json!({"repo": REPO})], false);
    configure(true, BASE);
    assert_eq!(lock().clock(), 0);
    ingest_page(
        &[
            serde_json::json!({"repo": REPO}),
            serde_json::json!({"seq": 2}),
        ],
        false,
    );
    assert_eq!(lock().clock(), 2);
    configure(false, BASE);
}
