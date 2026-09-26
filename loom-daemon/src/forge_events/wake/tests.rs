//! Tests for the Phase 2 early-tick wake seam (#8766).
//!
//! Split from `wake.rs` per the `forge_events/tests.rs` precedent so the
//! file-size ratchet measures production code on its own terms.
//!
//! # How "did / did not tick early" is asserted
//!
//! Every timing assertion runs on a **paused** tokio clock
//! (`#[tokio::test(start_paused = true)]`) against a ticker whose cadence is
//! an hour. Two properties follow, and both are load-bearing:
//!
//! - Paused time only auto-advances when the runtime has **nothing else to
//!   do**, so a bridge task with queued bus events always runs to completion
//!   before the clock moves. A wake that is owed is therefore always
//!   delivered — no sleep-and-hope.
//! - Nothing but the wake path can make [`EarlyTicker::tick`] return inside a
//!   sub-cadence [`tokio::time::timeout`]. A timeout means "the loop is
//!   waiting for its ordinary cadence" — exactly the pre-Phase-2 behaviour the
//!   default-off guarantee is about.
//!
//! The spacing floor is crossed with [`tokio::time::advance`] rather than a
//! real sleep, so a 30-second production floor is asserted in microseconds.

use super::*;

use serial_test::serial;

use crate::forge_events::{
    page_payload, FeedErrorClass, FeedPage, FeedPaths, FeedStatus, PollOutcome, ResolvedFeed,
    DEFAULT_POLL_INTERVAL_SECS,
};

// ============================================================================
// Fixtures
// ============================================================================

/// A cadence no test can accidentally wait out.
const NEVER: Duration = Duration::from_secs(3600);

/// The window a wake has to arrive in. On a paused clock this is consumed
/// instantly once the runtime goes idle, so it costs no wall time.
const SETTLE: Duration = Duration::from_millis(250);

/// The production spacing floor, used by the coalescing tests so they assert
/// the shipped bound rather than a test-only one.
const FLOOR: Duration = Duration::from_secs(DEFAULT_MIN_SPACING_SECS);

/// Build a `forge.event` payload the way Phase 1's `page_payload` does, from
/// synthetic events of the given types.
fn payload_of(types: &[&str]) -> serde_json::Value {
    let events: Vec<serde_json::Value> = types
        .iter()
        .enumerate()
        .map(|(i, name)| serde_json::json!({"seq": i as u64 + 1, "type": name}))
        .collect();
    page_payload("mac-studio", &events)
}

/// Publish one page prompt on the bus, exactly as Phase 1 does.
fn publish_page(bus: &EventBus, types: &[&str]) {
    let _ = bus.publish_generic(BUS_TOPIC, payload_of(types));
}

/// Consume the interval's free first tick so later assertions measure the
/// cadence rather than the boot tick.
async fn drain_first_tick(ticker: &mut EarlyTicker) {
    tokio::time::timeout(SETTLE, ticker.tick())
        .await
        .expect("an interval's first tick fires immediately");
}

/// An armed ticker whose boot tick is spent and whose spacing floor is clear,
/// so the next qualifying prompt is eligible to wake it.
async fn armed_and_ready(bus: &EventBus, floor: Duration) -> EarlyTicker {
    armed_and_ready_for(&WORK_FINDER, bus, floor).await
}

/// [`armed_and_ready`] for an arbitrary consumer.
async fn armed_and_ready_for(consumer: &Consumer, bus: &EventBus, floor: Duration) -> EarlyTicker {
    let mut ticker = EarlyTicker::armed(NEVER, bus, consumer.types, floor);
    drain_first_tick(&mut ticker).await;
    clear_floor(floor).await;
    ticker
}

/// Build a disarmed-or-armed ticker through the *production* constructor for
/// `consumer`, reading `root`'s config exactly as the daemon does.
fn for_consumer(consumer: &'static Consumer, bus: &EventBus, root: &Path) -> EarlyTicker {
    EarlyTicker::for_consumer(consumer, NEVER, bus, root)
}

/// All three consumers, armed and past their spacing floor, on one bus — the
/// shape the "zero wakes" invariant tests use so a regression that only leaked
/// through one consumer still fails.
async fn armed_trio(bus: &EventBus) -> Vec<(&'static str, EarlyTicker)> {
    let mut trio = Vec::new();
    for consumer in ALL_CONSUMERS {
        trio.push((consumer.name, armed_and_ready_for(consumer, bus, FLOOR).await));
    }
    trio
}

/// Assert none of [`armed_trio`]'s tickers woke, and none even saw a prompt.
async fn assert_trio_never_wakes(trio: &mut [(&'static str, EarlyTicker)], why: &str) {
    for (name, ticker) in trio.iter_mut() {
        assert!(
            !ticked_soon(ticker).await,
            "{name}: {why} must leave the loop on its ordinary cadence"
        );
        assert_eq!(ticker.counters().prompts(), 0, "{name}");
        assert_eq!(ticker.counters().early_ticks(), 0, "{name}");
    }
}

/// A page carrying every type any consumer acts on, so a "zero wakes" claim
/// cannot pass merely because the fixture was off-target.
fn every_wanted_type() -> Vec<&'static str> {
    let mut types: Vec<&'static str> = ALL_CONSUMERS
        .iter()
        .flat_map(|c| c.types.iter().copied())
        .collect();
    types.sort_unstable();
    types.dedup();
    types
}

/// Move the clock just past `floor` (never past the 1h cadence).
async fn clear_floor(floor: Duration) {
    tokio::time::advance(floor + Duration::from_secs(1)).await;
}

/// Did the ticker tick within [`SETTLE`]?
async fn ticked_soon(ticker: &mut EarlyTicker) -> bool {
    tokio::time::timeout(SETTLE, ticker.tick()).await.is_ok()
}

/// Clear every `forgeEvents.events.*` env override.
fn clear_env() {
    std::env::remove_var(MIN_SPACING_SECS_ENV);
    for consumer in ALL_CONSUMERS {
        std::env::remove_var(consumer.env);
    }
}

/// Write a `.loom/config.json` carrying `forgeEvents.events` verbatim.
fn write_events_config(root: &Path, events_json: &str) {
    std::fs::create_dir_all(root.join(".loom")).expect("mkdir .loom");
    std::fs::write(
        root.join(".loom/config.json"),
        format!(r#"{{"forgeEvents":{{"enabled":true,"events":{events_json}}}}}"#),
    )
    .expect("write config");
}

// ============================================================================
// Config resolution (env > config > default, default OFF)
// ============================================================================

#[test]
#[serial]
fn every_consumer_is_off_by_default_and_resolves_env_over_config() {
    clear_env();
    // The whole-block default, asserted for every consumer at once: this is
    // the ADR-0021 "off by default at merge" guarantee, and a fourth consumer
    // added without a default-off flag fails here rather than in production.
    for consumer in ALL_CONSUMERS {
        assert!(
            !resolve_enabled(consumer, &ConsumerConfig::default()),
            "{}: an absent forgeEvents.events block must leave the consumer off",
            consumer.name
        );
    }

    let all_on = ConsumerConfig {
        work_finder_tick: Some(true),
        queue_head_wake: Some(true),
        in_flight_pr_watch: Some(true),
        min_spacing_secs: None,
    };
    for consumer in ALL_CONSUMERS {
        assert!(resolve_enabled(consumer, &all_on), "{}", consumer.name);

        // Env wins both ways round, and each consumer has its OWN variable —
        // arming one must never arm another.
        std::env::set_var(consumer.env, "0");
        assert!(!resolve_enabled(consumer, &all_on), "{}", consumer.name);
        std::env::set_var(consumer.env, "true");
        assert!(resolve_enabled(consumer, &ConsumerConfig::default()), "{}", consumer.name);
        for other in ALL_CONSUMERS.iter().filter(|o| o.env != consumer.env) {
            assert!(
                !resolve_enabled(other, &ConsumerConfig::default()),
                "{} must not be armed by {}'s env override",
                other.name,
                consumer.name
            );
        }
        clear_env();
    }
}

#[test]
fn the_three_consumers_have_distinct_flags_envs_and_names() {
    // A copy-paste slip that gave two consumers the same key would silently
    // make one un-disable-able; the per-loop opt-out is the invariant here.
    for (i, a) in ALL_CONSUMERS.iter().enumerate() {
        for b in &ALL_CONSUMERS[i + 1..] {
            assert_ne!(a.config_key, b.config_key);
            assert_ne!(a.env, b.env);
            assert_ne!(a.name, b.name);
        }
    }
    assert_eq!(ALL_CONSUMERS.len(), 3, "Phase 2 ships exactly three consumers");
}

#[test]
#[serial]
fn the_spacing_floor_resolves_env_over_config_over_default() {
    clear_env();
    assert_eq!(resolve_min_spacing(&ConsumerConfig::default()), FLOOR);

    let config = ConsumerConfig {
        min_spacing_secs: Some(90),
        ..ConsumerConfig::default()
    };
    assert_eq!(resolve_min_spacing(&config), Duration::from_secs(90));

    std::env::set_var(MIN_SPACING_SECS_ENV, "5");
    assert_eq!(resolve_min_spacing(&config), Duration::from_secs(5));

    // A zero floor would remove the rate bound entirely, so it is dropped
    // rather than honoured.
    std::env::set_var(MIN_SPACING_SECS_ENV, "0");
    assert_eq!(resolve_min_spacing(&config), Duration::from_secs(90));
    clear_env();
}

#[test]
#[serial]
fn a_repo_with_no_events_block_reads_as_all_none() {
    clear_env();
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(read_config(dir.path()), ConsumerConfig::default());
}

#[test]
#[serial]
fn read_config_maps_every_camel_case_key() {
    clear_env();
    let dir = tempfile::tempdir().expect("tempdir");
    write_events_config(
        dir.path(),
        r#"{"workFinderTick":true,"queueHeadWake":false,"inFlightPrWatch":true,
            "minSpacingSecs":45}"#,
    );
    assert_eq!(
        read_config(dir.path()),
        ConsumerConfig {
            work_finder_tick: Some(true),
            queue_head_wake: Some(false),
            in_flight_pr_watch: Some(true),
            min_spacing_secs: Some(45),
        }
    );
}

// ============================================================================
// Qualification — routing hints only
// ============================================================================

#[test]
fn only_a_claimable_shaped_page_from_the_feed_qualifies() {
    let wanted = WORK_FINDER_EVENT_TYPES;

    assert!(payload_qualifies(&payload_of(&["issues"]), wanted));
    assert!(payload_qualifies(&payload_of(&["issue_comment"]), wanted));
    // One qualifying type among several is enough.
    assert!(payload_qualifies(&payload_of(&["check_run", "issues", "push"]), wanted));

    // A page of types this consumer does not act on is not a prompt for it.
    assert!(!payload_qualifies(
        &payload_of(&["check_run", "check_suite", "pull_request"]),
        wanted
    ));
    // An empty page never publishes in Phase 1; the count guard is the
    // structural reason a zero-event payload could not wake anything anyway.
    assert!(!payload_qualifies(&payload_of(&[]), wanted));
    // Another subsystem's Generic payload on this topic is not a feed prompt.
    assert!(!payload_qualifies(
        &serde_json::json!({"source": "monitor-db", "count": 3, "types": ["issues"]}),
        wanted
    ));
}

#[test]
fn each_consumer_acts_on_its_own_event_shapes_only() {
    // Queue-head wake: a claim is released by a comment on the claimed issue
    // or by a PR reaching terminal state — not by a check completing, and not
    // by an issue merely being opened.
    assert!(payload_qualifies(&payload_of(&["issue_comment"]), QUEUE_HEAD.types));
    assert!(payload_qualifies(&payload_of(&["pull_request"]), QUEUE_HEAD.types));
    assert!(!payload_qualifies(
        &payload_of(&["check_run", "check_suite", "push"]),
        QUEUE_HEAD.types
    ));

    // In-flight PR watch: the PR itself, or its checks.
    assert!(payload_qualifies(&payload_of(&["pull_request"]), IN_FLIGHT_PR.types));
    assert!(payload_qualifies(&payload_of(&["check_run"]), IN_FLIGHT_PR.types));
    assert!(payload_qualifies(&payload_of(&["check_suite"]), IN_FLIGHT_PR.types));
    // An issue opened tells a PR watch nothing.
    assert!(!payload_qualifies(&payload_of(&["issues"]), IN_FLIGHT_PR.types));

    // The three sets are genuinely different — a page of `issues` alone must
    // reach exactly one consumer, which is the point of routing on types.
    let armed_by_issues: Vec<&str> = ALL_CONSUMERS
        .iter()
        .filter(|c| payload_qualifies(&payload_of(&["issues"]), c.types))
        .map(|c| c.name)
        .collect();
    assert_eq!(armed_by_issues, vec![WORK_FINDER.name]);
}

#[test]
fn only_a_generic_on_the_feed_topic_is_a_prompt() {
    let wanted = WORK_FINDER_EVENT_TYPES;

    assert!(event_qualifies(
        &Event::Generic {
            topic: BUS_TOPIC.to_string(),
            payload: payload_of(&["issues"]),
        },
        wanted
    ));
    // Right payload, wrong topic.
    assert!(!event_qualifies(
        &Event::Generic {
            topic: "sweep.global.dispatch".to_string(),
            payload: payload_of(&["issues"]),
        },
        wanted
    ));
    // A lag sentinel says prompts were dropped; the answer is the polling
    // floor, not a speculative wake on an unknown page.
    assert!(!event_qualifies(&Event::TopicLag { skipped: 42 }, wanted));
}

// ============================================================================
// Flag off => zero wakes (the default-off guarantee, asserted not trusted)
// ============================================================================

/// The Acceptance criterion "with all consumer flags off, behaviour is
/// identical to Phase 1", asserted per consumer rather than asserted once and
/// generalised by hope.
///
/// Both off-shapes are covered: an absent `forgeEvents.events` block and an
/// explicit `false`. Both must produce a ticker with **no bus subscription at
/// all** — `receiver_count() == 0` is the structural half of the claim, and
/// the 500-page burst that follows is the behavioural half.
async fn assert_flag_off_is_zero_wakes(consumer: &'static Consumer, events_json: Option<&str>) {
    clear_env();
    let dir = tempfile::tempdir().expect("tempdir");
    if let Some(json) = events_json {
        write_events_config(dir.path(), json);
    }
    let bus = EventBus::new();

    let mut ticker = for_consumer(consumer, &bus, dir.path());
    assert!(
        !ticker.is_armed(),
        "{}: the default-off flag must leave the ticker disarmed",
        consumer.name
    );
    assert_eq!(
        bus.receiver_count(),
        0,
        "{}: a disarmed ticker must not subscribe to the bus at all",
        consumer.name
    );

    drain_first_tick(&mut ticker).await;
    clear_floor(FLOOR).await;

    // A burst carrying every type ANY consumer acts on, so the zero-wake
    // result cannot be an artifact of an off-target fixture.
    let types = every_wanted_type();
    for _ in 0..500 {
        publish_page(&bus, &types);
    }

    assert!(
        !ticked_soon(&mut ticker).await,
        "{}: flag off — the loop must wait for its ordinary cadence",
        consumer.name
    );
    assert_eq!(ticker.counters().early_ticks(), 0, "{}", consumer.name);
    assert_eq!(ticker.counters().prompts(), 0, "{}", consumer.name);
}

#[tokio::test(start_paused = true)]
#[serial]
async fn work_finder_flag_off_never_subscribes_and_never_wakes() {
    assert_flag_off_is_zero_wakes(&WORK_FINDER, None).await;
    // An explicit `false` is as off as an absent block.
    assert_flag_off_is_zero_wakes(&WORK_FINDER, Some(r#"{"workFinderTick":false}"#)).await;
}

#[tokio::test(start_paused = true)]
#[serial]
async fn queue_head_flag_off_never_subscribes_and_never_wakes() {
    assert_flag_off_is_zero_wakes(&QUEUE_HEAD, None).await;
    assert_flag_off_is_zero_wakes(&QUEUE_HEAD, Some(r#"{"queueHeadWake":false}"#)).await;
}

#[tokio::test(start_paused = true)]
#[serial]
async fn in_flight_pr_flag_off_never_subscribes_and_never_wakes() {
    assert_flag_off_is_zero_wakes(&IN_FLIGHT_PR, None).await;
    assert_flag_off_is_zero_wakes(&IN_FLIGHT_PR, Some(r#"{"inFlightPrWatch":false}"#)).await;
}

/// Arming one consumer must not arm another — the per-loop opt-out is
/// per-loop, so a fleet can run exactly one of the three.
#[tokio::test(start_paused = true)]
#[serial]
async fn arming_one_consumer_leaves_the_other_two_disarmed() {
    for consumer in ALL_CONSUMERS {
        clear_env();
        let dir = tempfile::tempdir().expect("tempdir");
        write_events_config(
            dir.path(),
            &format!(r#"{{"{}":true,"minSpacingSecs":30}}"#, consumer.config_key),
        );
        let bus = EventBus::new();

        let armed = for_consumer(consumer, &bus, dir.path());
        assert!(armed.is_armed(), "{} must be armed by its own key", consumer.name);

        for other in ALL_CONSUMERS
            .iter()
            .filter(|o| o.config_key != consumer.config_key)
        {
            let other_ticker = for_consumer(other, &bus, dir.path());
            assert!(
                !other_ticker.is_armed(),
                "{} must stay disarmed when only {} is on",
                other.name,
                consumer.name
            );
        }
        assert_eq!(
            bus.receiver_count(),
            1,
            "{}: exactly one subscription, from the one armed consumer",
            consumer.name
        );
    }
    clear_env();
}

/// The mirror image of the flag-off tests: each consumer's own key, set in
/// config, arms its own loop and a matching page ticks it early.
#[tokio::test(start_paused = true)]
#[serial]
async fn each_consumers_flag_on_in_config_arms_that_loop() {
    for consumer in ALL_CONSUMERS {
        clear_env();
        let dir = tempfile::tempdir().expect("tempdir");
        write_events_config(
            dir.path(),
            &format!(r#"{{"{}":true,"minSpacingSecs":30}}"#, consumer.config_key),
        );

        let bus = EventBus::new();
        let mut ticker = for_consumer(consumer, &bus, dir.path());
        assert!(ticker.is_armed(), "{}", consumer.name);
        assert_eq!(bus.receiver_count(), 1, "{}", consumer.name);

        drain_first_tick(&mut ticker).await;
        clear_floor(FLOOR).await;
        publish_page(&bus, &[consumer.types[0]]);

        assert!(ticked_soon(&mut ticker).await, "{}", consumer.name);
        assert_eq!(ticker.counters().early_ticks(), 1, "{}", consumer.name);
    }
    clear_env();
}

// ============================================================================
// Armed: a qualifying prompt ticks the loop early
// ============================================================================

#[tokio::test(start_paused = true)]
async fn an_armed_ticker_wakes_on_a_claimable_shaped_page() {
    let bus = EventBus::new();
    let mut ticker = armed_and_ready(&bus, FLOOR).await;
    assert!(ticker.is_armed());

    publish_page(&bus, &["issues"]);
    assert!(ticked_soon(&mut ticker).await, "a qualifying page must tick the loop early");
    assert_eq!(ticker.counters().early_ticks(), 1);
    assert_eq!(ticker.counters().prompts(), 1);
    assert_eq!(ticker.counters().throttled(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_page_of_types_this_consumer_does_not_act_on_produces_no_wake() {
    let bus = EventBus::new();
    let mut ticker = armed_and_ready(&bus, FLOOR).await;

    publish_page(&bus, &["check_run", "check_suite", "pull_request"]);
    assert!(
        !ticked_soon(&mut ticker).await,
        "a page with no claimable-shaped event must leave the cadence alone"
    );
    assert_eq!(ticker.counters().prompts(), 0);
    assert_eq!(ticker.counters().early_ticks(), 0);
}

// ============================================================================
// Burst coalescing — a burst costs one extra tick, not one per event
// ============================================================================

/// The Acceptance criterion "per-loop wake rate under a synthetic burst is
/// bounded by coalescing — one tick per loop", per consumer.
async fn assert_burst_costs_one_tick(consumer: &Consumer) {
    let bus = EventBus::new();
    let mut ticker = armed_and_ready_for(consumer, &bus, FLOOR).await;

    // 500 separate page prompts — strictly harder than the "500 events in one
    // page" case, which Phase 1 already collapses into a single publication.
    for _ in 0..500 {
        publish_page(&bus, &[consumer.types[0]]);
    }

    assert!(
        ticked_soon(&mut ticker).await,
        "{}: the burst must produce an early tick",
        consumer.name
    );
    assert!(
        !ticked_soon(&mut ticker).await,
        "{}: the rest of the burst must be coalesced away, not replayed as ticks",
        consumer.name
    );

    assert_eq!(
        ticker.counters().early_ticks(),
        1,
        "{}: a 500-prompt burst costs the loop exactly one extra tick",
        consumer.name
    );
    assert_eq!(
        ticker.counters().prompts(),
        500,
        "{}: the bridge did observe the whole burst — coalescing is the seam's \
         doing, not an accident of delivery",
        consumer.name
    );
    assert!(
        ticker.counters().throttled() >= 1,
        "{}: the spacing floor is what bounds the wake rate, and it fired",
        consumer.name
    );
}

#[tokio::test(start_paused = true)]
async fn a_five_hundred_page_burst_costs_the_work_finder_exactly_one_early_tick() {
    assert_burst_costs_one_tick(&WORK_FINDER).await;
}

#[tokio::test(start_paused = true)]
async fn a_five_hundred_page_burst_costs_the_queue_head_exactly_one_early_tick() {
    assert_burst_costs_one_tick(&QUEUE_HEAD).await;
}

#[tokio::test(start_paused = true)]
async fn a_five_hundred_page_burst_costs_the_pr_watch_exactly_one_early_tick() {
    assert_burst_costs_one_tick(&IN_FLIGHT_PR).await;
}

/// Three armed consumers sharing one bus: a burst is coalesced **per loop**,
/// not once globally. Each loop that cares owes itself exactly one extra tick,
/// and a loop that does not care owes itself none.
#[tokio::test(start_paused = true)]
async fn a_shared_burst_coalesces_independently_for_each_armed_loop() {
    let bus = EventBus::new();
    let mut work_finder = armed_and_ready_for(&WORK_FINDER, &bus, FLOOR).await;
    let mut queue_head = armed_and_ready_for(&QUEUE_HEAD, &bus, FLOOR).await;
    let mut pr_watch = armed_and_ready_for(&IN_FLIGHT_PR, &bus, FLOOR).await;
    assert_eq!(bus.receiver_count(), 3);

    // `issue_comment` is the one type both the work finder and the queue-head
    // consumer act on, and that the PR watch does not.
    for _ in 0..500 {
        publish_page(&bus, &["issue_comment"]);
    }

    assert!(ticked_soon(&mut work_finder).await);
    assert!(ticked_soon(&mut queue_head).await);
    assert!(
        !ticked_soon(&mut pr_watch).await,
        "a type the PR watch does not act on must leave it on its cadence"
    );

    assert_eq!(work_finder.counters().early_ticks(), 1);
    assert_eq!(queue_head.counters().early_ticks(), 1);
    assert_eq!(pr_watch.counters().early_ticks(), 0);
    assert_eq!(pr_watch.counters().prompts(), 0);
}

#[tokio::test(start_paused = true)]
async fn the_spacing_floor_bounds_the_wake_rate_after_any_tick() {
    let bus = EventBus::new();
    let mut ticker = armed_and_ready(&bus, FLOOR).await;

    publish_page(&bus, &["issues"]);
    assert!(ticked_soon(&mut ticker).await);
    assert_eq!(ticker.counters().early_ticks(), 1);

    // A fresh, genuinely-later prompt still cannot tick inside the floor.
    publish_page(&bus, &["issue_comment"]);
    assert!(
        !ticked_soon(&mut ticker).await,
        "no early tick may land within the spacing floor of the previous tick"
    );
    assert_eq!(ticker.counters().early_ticks(), 1);
    assert_eq!(ticker.counters().prompts(), 2);
    assert!(ticker.counters().throttled() >= 1);

    // …and it is a floor, not a one-shot: past it, prompts wake again.
    clear_floor(FLOOR).await;
    publish_page(&bus, &["issues"]);
    assert!(ticked_soon(&mut ticker).await);
    assert_eq!(ticker.counters().early_ticks(), 2);
}

// ============================================================================
// A feed that never verified produces zero wakes (invariant 3, end to end)
// ============================================================================

/// Drive Phase 1's own `FeedClient` with pages that fail host verification and
/// assert the Phase 2 seam never sees a prompt.
///
/// Deliberately end-to-end through `apply_page` rather than a hand-built "no
/// publication" assertion: the guarantee is that the *publisher* refuses to
/// publish, so the consumer needs no rule of its own. A regression that made
/// `apply_page` publish before verifying the host would be invisible to a
/// consumer-only test and fails this one.
#[tokio::test(start_paused = true)]
async fn a_host_mismatched_feed_produces_zero_wakes() {
    let bus = EventBus::new();
    let mut trio = armed_trio(&bus).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let key_file = dir.path().join("key");
    std::fs::write(&key_file, "super-secret-key\n").expect("write key");
    let feed = ResolvedFeed {
        endpoint: "https://events.internal".to_string(),
        host_id: "mac-studio".to_string(),
        key_file,
        poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
        page_size: 100,
        paths: FeedPaths::in_dir(dir.path().to_path_buf()),
    };
    let status = Arc::new(FeedStatus::started(&feed, 0));
    let mut client =
        crate::forge_events::FeedClient::new(feed, bus.clone(), status).expect("construct client");

    // Well-formed, carrying a type every consumer acts on, but echoing
    // somebody else's host id.
    let foreign = FeedPage {
        host_id: Some("someone-elses-laptop".to_string()),
        cursor: Some(9),
        events: vec![
            serde_json::json!({"seq": 8, "type": "issues"}),
            serde_json::json!({"seq": 9, "type": "issue_comment"}),
            serde_json::json!({"seq": 10, "type": "pull_request"}),
            serde_json::json!({"seq": 11, "type": "check_run"}),
        ],
        ..FeedPage::default()
    };
    assert!(matches!(
        client.apply_page(foreign),
        PollOutcome::Failed(FeedErrorClass::HostMismatch)
    ));

    // No host id to cross-check at all — the other mismatch shape.
    let anonymous = FeedPage {
        host_id: None,
        cursor: Some(12),
        events: vec![
            serde_json::json!({"seq": 12, "type": "issues"}),
            serde_json::json!({"seq": 13, "type": "check_suite"}),
        ],
        ..FeedPage::default()
    };
    assert!(matches!(
        client.apply_page(anonymous),
        PollOutcome::Failed(FeedErrorClass::HostMismatch)
    ));

    assert_trio_never_wakes(&mut trio, "a feed that never verified").await;
}

/// A `backoff` feed is the same shape from the consumer's side: the poll never
/// produced an applicable page, so nothing was published. Asserted through the
/// protocol-failure path that promotes to `backoff` after three tries.
#[tokio::test(start_paused = true)]
async fn a_failing_feed_that_backs_off_produces_zero_wakes() {
    let bus = EventBus::new();
    let mut trio = armed_trio(&bus).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let key_file = dir.path().join("key");
    std::fs::write(&key_file, "super-secret-key\n").expect("write key");
    let feed = ResolvedFeed {
        endpoint: "https://events.internal".to_string(),
        host_id: "mac-studio".to_string(),
        key_file,
        poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
        page_size: 100,
        paths: FeedPaths::in_dir(dir.path().to_path_buf()),
    };
    let status = Arc::new(FeedStatus::started(&feed, 0));
    let mut client = crate::forge_events::FeedClient::new(feed, bus.clone(), status.clone())
        .expect("construct client");

    // A page with no cursor is a protocol failure; three of them promote the
    // feed to `backoff`.
    for _ in 0..crate::forge_events::BACKOFF_FAILURE_STREAK {
        let cursorless = FeedPage {
            host_id: Some("mac-studio".to_string()),
            cursor: None,
            events: vec![
                serde_json::json!({"seq": 1, "type": "issues"}),
                serde_json::json!({"seq": 2, "type": "pull_request"}),
                serde_json::json!({"seq": 3, "type": "check_run"}),
            ],
            ..FeedPage::default()
        };
        assert!(matches!(
            client.apply_page(cursorless),
            PollOutcome::Failed(FeedErrorClass::Protocol)
        ));
    }
    assert_eq!(
        status.snapshot().state,
        crate::types::ForgeEventsState::Backoff,
        "three protocol failures must promote the feed to backoff"
    );

    assert_trio_never_wakes(&mut trio, "a backed-off feed").await;
}

// ============================================================================
// Lifecycle
// ============================================================================

#[tokio::test]
async fn dropping_the_ticker_releases_its_subscription() {
    let bus = EventBus::new();
    let ticker = EarlyTicker::armed(NEVER, &bus, WORK_FINDER_EVENT_TYPES, FLOOR);
    assert_eq!(bus.receiver_count(), 1);

    drop(ticker);
    // The bridge is aborted on drop; give the runtime a moment to reap it.
    for _ in 0..50 {
        if bus.receiver_count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        bus.receiver_count(),
        0,
        "a dropped ticker must not leave a subscriber on the bus"
    );
}

#[tokio::test]
async fn a_plain_ticker_carries_no_wake_state() {
    let ticker = EarlyTicker::plain(NEVER);
    assert!(!ticker.is_armed());
    assert_eq!(ticker.counters().prompts(), 0);
    assert_eq!(ticker.counters().early_ticks(), 0);
    assert_eq!(ticker.counters().throttled(), 0);
}
