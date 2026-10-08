//! Unit tests for the roll-window schedule, offset, config precedence, restart-path
//! selection and the one-arm-per-window gate. Injected clock throughout: no sleeps.

use super::*;
use crate::auto_update::supersede::ArmedRoll;
use crate::auto_update::RollTarget;
use serial_test::serial;
use std::collections::HashSet;
use std::sync::Mutex;

const HOUR: u64 = 3600;

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).unwrap()
}

fn tuning(period: u64, offset: u64) -> RollWindowTuning {
    RollWindowTuning {
        period: Some(Duration::from_secs(period)),
        offset: Duration::from_secs(offset),
        open_for: Duration::from_secs(1800),
        launchd_live_reload: false,
    }
}

fn fetch(version: &str) -> TickDecision {
    TickDecision::FetchArtifact {
        version: version.to_string(),
        tag: format!("v{version}"),
        why: "test".to_string(),
        low_priority: false,
    }
}

/// A trigger with an optional armed roll.
#[derive(Default)]
struct FakeTrigger {
    armed: Mutex<Option<ArmedRoll>>,
}

impl FakeTrigger {
    fn with(target: &str, committed: bool) -> Self {
        Self {
            armed: Mutex::new(Some(ArmedRoll {
                target: Some(target.to_string()),
                committed,
                then_exit: false,
            })),
        }
    }
}

impl RollTrigger for FakeTrigger {
    fn trigger_pause_roll(&self, _target: &RollTarget) -> bool {
        true
    }
    fn armed_roll(&self) -> Option<ArmedRoll> {
        self.armed.lock().unwrap().clone()
    }
}

fn is_skip(decision: &TickDecision) -> bool {
    matches!(decision, TickDecision::Skip(_))
}

// ---- schedule arithmetic -------------------------------------------------

#[test]
fn window_opens_at_offset_plus_k_periods() {
    let (period, offset, open_for) = (
        Duration::from_secs(6 * HOUR),
        Duration::from_secs(HOUR),
        Duration::from_secs(1800),
    );
    // Windows start at 01:00, 07:00, 13:00, ... (UTC epoch aligned).
    assert!(window_is_open(at(HOUR as i64), period, offset, open_for));
    assert!(window_is_open(at(HOUR as i64 + 1799), period, offset, open_for));
    assert!(!window_is_open(at(HOUR as i64 + 1800), period, offset, open_for));
    assert!(!window_is_open(at(HOUR as i64 - 1), period, offset, open_for));
    assert!(window_is_open(at(7 * HOUR as i64 + 5), period, offset, open_for));
}

#[test]
fn next_window_open_is_strictly_in_the_future() {
    let (period, offset) = (Duration::from_secs(6 * HOUR), Duration::from_secs(HOUR));
    assert_eq!(next_window_open(at(0), period, offset), at(HOUR as i64));
    // Exactly at a start, the *next* one is a full period away.
    assert_eq!(next_window_open(at(HOUR as i64), period, offset), at(7 * HOUR as i64));
    assert_eq!(next_window_open(at(HOUR as i64 + 10), period, offset), at(7 * HOUR as i64));
}

#[test]
fn pre_epoch_style_negative_offsets_never_panic() {
    // `now` before the first window start still resolves (div_euclid, not `/`).
    let (period, offset) = (Duration::from_secs(HOUR), Duration::from_secs(1800));
    assert_eq!(window_index(at(0), period, offset), -1);
    assert_eq!(next_window_open(at(0), period, offset), at(1800));
}

// ---- offset --------------------------------------------------------------

#[test]
fn derived_offset_is_stable_and_within_the_period() {
    let period = 6 * HOUR;
    for host in [
        "loom-worker-1",
        "loom-worker-2",
        "robb-pro",
        "",
        "host with spaces",
    ] {
        let first = derive_offset(host, period);
        assert_eq!(first, derive_offset(host, period), "same host => same offset");
        assert!(first < period);
    }
    // Whitespace around the id is not significant (host ids come from `hostname`).
    assert_eq!(derive_offset("robb-pro\n", period), derive_offset("robb-pro", period));
    assert_eq!(derive_offset("anything", 0), 0);
}

#[test]
fn derived_offsets_spread_distinct_hosts_across_the_period() {
    let period = 6 * HOUR;
    let offsets: HashSet<u64> = (0..64)
        .map(|n| derive_offset(&format!("host-{n:08x}"), period))
        .collect();
    assert!(
        offsets.len() >= 60,
        "64 hosts should land on nearly-distinct offsets: {}",
        offsets.len()
    );
    // And they cover the period rather than clustering in one corner of it.
    let buckets: HashSet<u64> = offsets.iter().map(|o| o * 6 / period).collect();
    assert_eq!(buckets.len(), 6, "every sixth of the period holds a host");
}

// ---- config precedence ---------------------------------------------------

fn resolve(
    cfg: &RollWindowConfig,
    env_period: Option<u64>,
    env_offset: Option<u64>,
) -> RollWindowTuning {
    RollWindowTuning::resolve_with(
        cfg,
        env_period,
        env_offset,
        None,
        "host-a",
        Duration::from_secs(900),
    )
}

#[test]
fn absent_config_means_no_window() {
    let t = resolve(&RollWindowConfig::default(), None, None);
    assert_eq!(t.period, None);
    assert!(!t.launchd_live_reload);
    assert_eq!(t.describe(), "rollWindow=off");
}

#[test]
fn period_precedence_is_env_over_config_over_default() {
    let cfg = RollWindowConfig {
        period_secs: Some(4 * HOUR),
        ..RollWindowConfig::default()
    };
    assert_eq!(resolve(&cfg, None, None).period, Some(Duration::from_secs(4 * HOUR)));
    assert_eq!(resolve(&cfg, Some(2 * HOUR), None).period, Some(Duration::from_secs(2 * HOUR)));
    // A zero env value is dropped before it reaches `resolve_with` (see the env test
    // below); a zero reaching it directly must still never enable a 0s window.
    assert_eq!(resolve(&cfg, Some(0), None).period, Some(Duration::from_secs(4 * HOUR)));
    assert_eq!(resolve(&RollWindowConfig::default(), Some(0), None).period, None);
}

#[test]
fn offset_precedence_explicit_wins_over_derived_and_is_always_below_the_period() {
    let period = 6 * HOUR;
    let derived = derive_offset("host-a", period);
    let cfg = RollWindowConfig {
        period_secs: Some(period),
        ..RollWindowConfig::default()
    };
    assert_eq!(resolve(&cfg, None, None).offset, Duration::from_secs(derived));

    let cfg = RollWindowConfig {
        offset_secs: Some(HOUR),
        ..cfg
    };
    assert_eq!(resolve(&cfg, None, None).offset, Duration::from_secs(HOUR), "config > derived");
    assert_eq!(
        resolve(&cfg, None, Some(2 * HOUR)).offset,
        Duration::from_secs(2 * HOUR),
        "env > config"
    );
    // An out-of-range explicit offset is reduced into the period.
    assert_eq!(resolve(&cfg, None, Some(period + 5)).offset, Duration::from_secs(5));
    for raw in [1, period - 1, period, period * 3 + 7] {
        let t = resolve(&cfg, None, Some(raw));
        assert!(t.offset < t.period.unwrap());
    }
}

#[test]
fn open_window_covers_at_least_two_ticks_but_never_the_whole_period_plus() {
    let cfg = RollWindowConfig {
        period_secs: Some(6 * HOUR),
        ..RollWindowConfig::default()
    };
    let t = RollWindowTuning::resolve_with(&cfg, None, None, None, "h", Duration::from_secs(900));
    assert_eq!(t.open_for, Duration::from_secs(1800));
    let t = RollWindowTuning::resolve_with(&cfg, None, None, None, "h", Duration::from_secs(60));
    assert_eq!(t.open_for, Duration::from_secs(MIN_OPEN_SECS));
    let short = RollWindowConfig {
        period_secs: Some(300),
        ..RollWindowConfig::default()
    };
    let t = RollWindowTuning::resolve_with(&short, None, None, None, "h", Duration::from_secs(900));
    assert_eq!(t.open_for, Duration::from_secs(300), "capped at the period");
}

#[test]
fn live_reload_flag_precedence() {
    let on = RollWindowConfig {
        launchd_live_reload: Some(true),
        ..RollWindowConfig::default()
    };
    let r = |cfg: &RollWindowConfig, env| {
        RollWindowTuning::resolve_with(cfg, None, None, env, "h", Duration::from_secs(900))
            .launchd_live_reload
    };
    assert!(!r(&RollWindowConfig::default(), None), "default is OFF");
    assert!(r(&on, None));
    assert!(!r(&on, Some(false)), "env > config");
    assert!(r(&RollWindowConfig::default(), Some(true)));
}

#[test]
fn config_block_parsing_drops_zero_and_invalid_values() {
    let block = serde_json::json!({
        "rollWindowSecs": 21600, "rollWindowOffsetSecs": 0, "launchdLiveReload": "yes"
    });
    let cfg = RollWindowConfig::from_block(&block);
    assert_eq!(cfg.period_secs, Some(21_600));
    assert_eq!(cfg.offset_secs, None, "zero offset => derive");
    assert_eq!(cfg.launchd_live_reload, None, "non-bool => default");
    let cfg = RollWindowConfig::from_block(&serde_json::json!({
        "rollWindowSecs": -5, "launchdLiveReload": true
    }));
    assert_eq!(cfg.period_secs, None);
    assert_eq!(cfg.launchd_live_reload, Some(true));
}

#[test]
#[serial(loom_auto_update_env)]
fn env_tier_reaches_resolve_and_zero_or_garbage_falls_through() {
    let cfg = RollWindowConfig {
        period_secs: Some(4 * HOUR),
        ..RollWindowConfig::default()
    };
    std::env::set_var(ROLL_WINDOW_SECS_ENV, "7200");
    std::env::set_var(ROLL_WINDOW_OFFSET_SECS_ENV, "600");
    let t = RollWindowTuning::resolve(&cfg, Duration::from_secs(900));
    assert_eq!(t.period, Some(Duration::from_secs(7200)));
    assert_eq!(t.offset, Duration::from_secs(600));
    std::env::set_var(ROLL_WINDOW_SECS_ENV, "0");
    std::env::set_var(ROLL_WINDOW_OFFSET_SECS_ENV, "not-a-number");
    let t = RollWindowTuning::resolve(&cfg, Duration::from_secs(900));
    assert_eq!(t.period, Some(Duration::from_secs(4 * HOUR)));
    std::env::remove_var(ROLL_WINDOW_SECS_ENV);
    std::env::remove_var(ROLL_WINDOW_OFFSET_SECS_ENV);
    std::env::remove_var(LAUNCHD_LIVE_RELOAD_ENV);
}

// ---- restart path --------------------------------------------------------

#[test]
fn live_reload_is_selected_only_on_launchd_with_the_opt_in() {
    assert_eq!(select_restart_path(Platform::Launchd, false), RestartPath::BoundedDrain);
    assert_eq!(select_restart_path(Platform::Launchd, true), RestartPath::LiveReload);
    // systemd (and anything unknown) always drains, opt-in or not.
    assert_eq!(select_restart_path(Platform::Systemd, true), RestartPath::BoundedDrain);
    assert_eq!(select_restart_path(Platform::Systemd, false), RestartPath::BoundedDrain);
    assert_eq!(select_restart_path(Platform::Other, true), RestartPath::BoundedDrain);
}

// ---- the gate ------------------------------------------------------------

/// Window 0 opens at t=1h, period 6h, open 30m.
fn gate() -> WindowGate {
    WindowGate::new(tuning(6 * HOUR, HOUR))
}

#[test]
fn disabled_gate_is_inert() {
    let mut g = WindowGate::new(RollWindowTuning::default());
    let t = FakeTrigger::default();
    assert_eq!(g.begin_tick(at(5), &t, Duration::from_secs(600)), Duration::from_secs(600));
    assert!(matches!(g.gate(at(5), fetch("0.19.2")), TickDecision::FetchArtifact { .. }));
    assert!(matches!(
        g.gate(
            at(5),
            TickDecision::Rebuild {
                low_priority: false
            }
        ),
        TickDecision::Rebuild { .. }
    ));
    assert_eq!(g.status(), None);
}

#[test]
fn a_new_build_outside_the_window_arms_nothing() {
    let mut g = gate();
    let t = FakeTrigger::default();
    // t=20m: window 0 has not opened yet.
    g.begin_tick(at(1200), &t, Duration::from_secs(600));
    let d = g.gate(at(1200), fetch("0.19.2"));
    let TickDecision::Skip(reason) = d else {
        panic!("must not arm outside the window")
    };
    assert!(reason.starts_with("scheduled wait"), "{reason}");
    // A newer release a few ticks later: still nothing.
    g.begin_tick(at(2100), &t, Duration::ZERO);
    assert!(is_skip(&g.gate(at(2100), fetch("0.19.3"))));
    // Status explains the wait and names the build that is waiting.
    let s = g.status().unwrap();
    assert_eq!(s.roll_target.as_deref(), Some("0.19.3"));
    assert_eq!(s.next_window_open, at(HOUR as i64));
    assert!(!s.window_open_now);
    assert!(s.deferral.unwrap().starts_with("scheduled wait"));
}

#[test]
fn at_window_open_the_roll_arms_against_the_current_build() {
    let mut g = gate();
    let t = FakeTrigger::default();
    // Two builds landed while the window was closed; only the latest is on disk
    // (what `decide` hands the gate) when the window opens.
    assert!(is_skip(&g.gate(at(600), fetch("0.19.2"))));
    g.begin_tick(at(HOUR as i64 + 60), &t, Duration::from_secs(600));
    let TickDecision::FetchArtifact { version, .. } = g.gate(at(HOUR as i64 + 60), fetch("0.19.3"))
    else {
        panic!("the window is open: the roll must arm");
    };
    assert_eq!(version, "0.19.3");
    assert_eq!(g.status().unwrap().deferral, None);
    // The settle gate is bypassed while windowed.
    assert_eq!(
        g.begin_tick(at(HOUR as i64 + 60), &t, Duration::from_secs(3600)),
        Duration::ZERO
    );
}

#[test]
fn a_source_rebuild_is_window_gated_too() {
    let mut g = gate();
    assert!(is_skip(&g.gate(
        at(10),
        TickDecision::Rebuild {
            low_priority: false
        }
    )));
    assert!(matches!(
        g.gate(
            at(HOUR as i64 + 1),
            TickDecision::Rebuild {
                low_priority: false
            }
        ),
        TickDecision::Rebuild { .. }
    ));
}

#[test]
fn non_arming_decisions_pass_through_untouched() {
    let mut g = gate();
    let skip = TickDecision::Skip("up to date".to_string());
    assert_eq!(g.gate(at(10), skip.clone()), skip);
    let warn = TickDecision::SkipWarn("stale repo".to_string());
    assert_eq!(g.gate(at(10), warn.clone()), warn);
    assert_eq!(g.status().unwrap().roll_target, None);
}

#[test]
fn a_roll_that_ended_without_a_restart_is_not_rearmed_until_the_next_window() {
    let mut g = gate();
    let open = HOUR as i64 + 60;
    // Window 0: the roll arms.
    let idle = FakeTrigger::default();
    g.begin_tick(at(open), &idle, Duration::ZERO);
    assert!(matches!(g.gate(at(open), fetch("0.19.3")), TickDecision::FetchArtifact { .. }));

    // The next tick sees it armed: dispatch is paused by the update.
    let pausing = FakeTrigger::with("v0.19.3@abc", false);
    g.begin_tick(at(open + 900), &pausing, Duration::ZERO);
    assert!(g.status().unwrap().dispatch_paused_by_update);

    // The roll ended without a restart (aborted before it stopped anything).
    // Several more ticks AND newer releases in the same window: never re-armed.
    for (n, version) in ["0.19.3", "0.19.4", "0.19.5"].into_iter().enumerate() {
        let now = at(open + 1200 + 60 * (n as i64 + 1));
        g.begin_tick(now, &idle, Duration::ZERO);
        assert!(!g.status().unwrap().dispatch_paused_by_update);
        let TickDecision::Skip(reason) = g.gate(now, fetch(version)) else {
            panic!("re-armed inside a window that was already used");
        };
        assert!(reason.contains("already armed"), "{reason}");
    }

    // Window 1 (t = 7h+): eligible again, against whatever is on disk then.
    let next = 7 * HOUR as i64 + 30;
    g.begin_tick(at(next), &idle, Duration::ZERO);
    let TickDecision::FetchArtifact { version, .. } = g.gate(at(next), fetch("0.19.6")) else {
        panic!("the next window must re-arm");
    };
    assert_eq!(version, "0.19.6");
}

#[test]
fn an_armed_roll_consumes_the_window_even_when_the_loop_never_saw_the_arm() {
    let mut g = gate();
    let open = HOUR as i64 + 60;
    let armed = FakeTrigger::with("v0.19.3@abc", false);
    g.begin_tick(at(open), &armed, Duration::ZERO);
    // The drain ended some other way (completed/aborted): same window, no re-arm.
    let idle = FakeTrigger::default();
    g.begin_tick(at(open + 300), &idle, Duration::ZERO);
    let TickDecision::Skip(reason) = g.gate(at(open + 300), fetch("0.19.4")) else {
        panic!("one arm per window");
    };
    assert!(reason.contains("already armed"), "{reason}");
}

#[test]
fn a_superseded_roll_may_retarget_once_inside_the_same_open_window() {
    let mut g = gate();
    let open = HOUR as i64 + 60;
    g.begin_tick(at(open), &FakeTrigger::default(), Duration::ZERO);
    assert!(matches!(g.gate(at(open), fetch("0.19.3")), TickDecision::FetchArtifact { .. }));
    // Next tick: #8514 discards the still-draining roll for a newer release.
    g.begin_tick(at(open + 300), &FakeTrigger::with("v0.19.3@abc", false), Duration::ZERO);
    g.allow_retarget();
    assert!(matches!(
        g.gate(at(open + 300), fetch("0.19.4")),
        TickDecision::FetchArtifact { .. }
    ));
    // The allowance is per tick: the next tick (new roll armed) cannot re-arm again.
    g.begin_tick(at(open + 600), &FakeTrigger::with("v0.19.4@def", false), Duration::ZERO);
    assert!(is_skip(&g.gate(at(open + 600), fetch("0.19.5"))));
}

#[test]
fn a_retarget_never_arms_after_the_window_has_closed() {
    let mut g = gate();
    let late = HOUR as i64 + 1800 + 5;
    g.begin_tick(at(late), &FakeTrigger::with("v0.19.3@abc", false), Duration::ZERO);
    g.allow_retarget();
    // Superseded after close: the stale target is dropped, the newest waits.
    let TickDecision::Skip(reason) = g.gate(at(late), fetch("0.19.4")) else {
        panic!("a retarget must not arm after the window closed");
    };
    assert!(reason.contains("0.19.4"), "{reason}");
}

#[test]
fn an_operator_teardown_drain_is_never_counted_as_an_update_pause() {
    let mut g = gate();
    let teardown = FakeTrigger::default();
    *teardown.armed.lock().unwrap() = Some(ArmedRoll {
        target: None,
        committed: true,
        then_exit: true,
    });
    g.begin_tick(at(HOUR as i64 + 60), &teardown, Duration::ZERO);
    assert!(!g.status().unwrap().dispatch_paused_by_update);
    // An untargeted operator `restart --drain` is likewise left alone.
    let operator = FakeTrigger::default();
    *operator.armed.lock().unwrap() = Some(ArmedRoll {
        target: None,
        committed: true,
        then_exit: false,
    });
    g.begin_tick(at(HOUR as i64 + 120), &operator, Duration::ZERO);
    assert!(!g.status().unwrap().dispatch_paused_by_update);
}

#[test]
fn status_reports_the_armed_target_and_the_restart_path() {
    let mut g = gate();
    g.begin_tick(at(HOUR as i64 + 60), &FakeTrigger::with("v0.19.3@abc", false), Duration::ZERO);
    let s = g.status().unwrap();
    assert_eq!(s.roll_target.as_deref(), Some("v0.19.3@abc"));
    assert!(s.dispatch_paused_by_update);
    assert!(s.window_open_now);
    assert_eq!((s.period_secs, s.offset_secs), (6 * HOUR, HOUR));
    assert_eq!(s.restart_path, "bounded_drain");
    // Round-trips on the wire (it rides inside `DaemonStatusReport`).
    let json = serde_json::to_string(&s).unwrap();
    assert_eq!(serde_json::from_str::<RollWindowStatus>(&json).unwrap(), s);
}
