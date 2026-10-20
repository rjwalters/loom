#![allow(clippy::unwrap_used)]

use super::*;
use serde_json::json;

/// A scripted source: the given values in order, then the last one forever.
struct Script(Vec<f64>, usize);

impl Script {
    fn new(v: &[f64]) -> Self {
        Self(v.to_vec(), 0)
    }
}

impl Rng for Script {
    fn unit(&mut self) -> f64 {
        let v = self.0[self.1.min(self.0.len() - 1)];
        self.1 += 1;
        v
    }
}

fn t(secs: u64) -> Mono {
    Mono(Duration::from_secs(secs))
}

fn d(secs: u64) -> Duration {
    Duration::from_secs(secs)
}

/// No jitter, no startup wait: delays are exactly nominal.
fn exact() -> Config {
    Config {
        jitter: 0.0,
        startup: Duration::ZERO,
        ..Config::default()
    }
}

fn ready(cfg: Config) -> Requester {
    let mut r = Requester::new(cfg, t(0), &mut Script::new(&[0.0]));
    r.on_capacity(2);
    r
}

#[test]
fn nominal_delays_are_30_60_120_then_hold_at_the_cap() {
    let c = Config::default();
    let got: Vec<_> = (0..6).map(|n| c.nominal(n)).collect();
    assert_eq!(got, [d(30), d(60), d(120), d(120), d(120), d(120)]);
    assert_eq!(c.nominal(u32::MAX), d(120));
}

#[test]
fn an_empty_answer_schedules_the_next_request_and_grows_the_streak() {
    let mut r = ready(exact());
    let mut rng = Script::new(&[0.0]);
    let mut now = t(0);
    for (empties, wait) in [(1, 30), (2, 60), (3, 120), (4, 120)] {
        assert_eq!(r.poll(now), Poll::Request { free_slots: 2 });
        r.on_answer(now, false, &mut rng);
        assert_eq!(r.streak(), empties);
        assert_eq!(r.poll(now), Poll::Wait(d(wait)));
        now = now.plus(d(wait));
    }
}

#[test]
fn work_resets_the_streak_and_is_due_at_once() {
    let mut rng = Script::new(&[0.0]);
    let mut r = ready(exact());
    for n in 0..3 {
        let now = t(1000 * (n + 1));
        assert!(matches!(r.poll(now), Poll::Request { .. }));
        r.on_answer(now, false, &mut rng);
    }
    assert_eq!(r.streak(), 3);
    assert!(matches!(r.poll(t(5000)), Poll::Request { .. }));
    r.on_answer(t(5000), true, &mut rng);
    assert_eq!(r.streak(), 0);
    assert_eq!(r.poll(t(5000)), Poll::Request { free_slots: 2 });
    r.on_answer(t(5000), false, &mut rng);
    assert_eq!(r.poll(t(5000)), Poll::Wait(d(30)), "back to the initial delay");
}

#[test]
fn repeated_capacity_checks_do_not_reset_or_wake() {
    let mut r = ready(exact());
    let mut rng = Script::new(&[0.0]);
    assert!(matches!(r.poll(t(0)), Poll::Request { .. }));
    r.on_answer(t(0), false, &mut rng);
    r.on_answer(t(0), false, &mut rng);
    let streak = r.streak();
    for _ in 0..50 {
        r.on_capacity(2);
        r.on_capacity(1);
        assert_eq!(r.streak(), streak);
    }
    assert!(matches!(r.poll(t(1)), Poll::Wait(_)), "a lower count is no wake");
}

#[test]
fn no_capacity_stops_requests_and_regaining_it_wakes_a_coalesced_request() {
    let mut r = ready(exact());
    let mut rng = Script::new(&[0.0]);
    assert!(matches!(r.poll(t(0)), Poll::Request { .. }));
    r.on_answer(t(0), false, &mut rng);
    r.on_capacity(0);
    assert_eq!(r.poll(t(1000)), Poll::Idle, "due long ago, but no capacity");
    // A storm of rises coalesces into one wake.
    for _ in 0..20 {
        r.on_capacity(1);
        r.on_capacity(0);
    }
    r.on_capacity(3);
    // 29 s after the last request: under the 30 s wake floor, so it waits.
    assert_eq!(r.poll(t(29)), Poll::Wait(d(1)));
    assert_eq!(r.poll(t(30)), Poll::Request { free_slots: 3 });
    assert_eq!(r.poll(t(30)), Poll::Idle, "one in flight");
}

#[test]
fn a_wake_is_spaced_from_the_previous_request_by_the_floor() {
    let c = Config::default();
    assert_eq!(c.wake_floor(), d(15));
    let mut r = ready(Config {
        startup: Duration::ZERO,
        ..c
    });
    let mut rng = Script::new(&[0.0]);
    assert!(matches!(r.poll(t(0)), Poll::Request { .. }));
    r.on_answer(t(0), false, &mut rng);
    r.on_capacity(0);
    r.on_capacity(1);
    assert_eq!(r.poll(t(5)), Poll::Wait(d(10)));
    assert!(matches!(r.poll(t(15)), Poll::Request { .. }));
}

#[test]
fn one_request_in_flight_and_notifications_wait_for_the_answer() {
    let mut r = ready(exact());
    assert!(matches!(r.poll(t(0)), Poll::Request { .. }));
    assert!(r.in_flight());
    r.on_capacity(0);
    r.on_capacity(5);
    assert_eq!(r.poll(t(500)), Poll::Idle);
    r.on_answer(t(500), true, &mut Script::new(&[0.0]));
    assert!(!r.in_flight());
    assert_eq!(r.poll(t(500)), Poll::Request { free_slots: 5 });
}

#[test]
fn startup_is_randomized_and_a_wake_cannot_beat_it() {
    let cfg = Config::default();
    for (u, expect) in [(0.0, 0), (0.5, 15), (0.999, 29)] {
        let mut r = Requester::new(cfg, t(0), &mut Script::new(&[u]));
        r.on_capacity(1);
        match r.poll(t(0)) {
            Poll::Request { .. } => assert_eq!(expect, 0),
            Poll::Wait(w) => assert_eq!(w.as_secs(), expect),
            Poll::Idle => panic!("idle"),
        }
    }
    // Capacity noise at start does not skip the startup delay.
    let mut r = Requester::new(cfg, t(0), &mut Script::new(&[0.5]));
    r.on_capacity(1);
    r.on_capacity(0);
    r.on_capacity(1);
    assert_eq!(r.poll(t(0)), Poll::Wait(d(15)));
    assert!(matches!(r.poll(t(15)), Poll::Request { .. }));
}

#[test]
fn jitter_is_bounded_by_the_cap_and_kept_at_it() {
    let c = Config::default();
    let mut lows = std::collections::BTreeSet::new();
    for step in 0..6 {
        for u in [0.0, 0.25, 0.5, 0.75, 0.9999] {
            let delay = c.draw(step, &mut Script::new(&[u]));
            assert!(delay <= c.max, "step {step}: {delay:?} over the cap");
            assert!(delay >= c.min_delay(step) && delay <= c.max_delay(step));
            if step >= 2 {
                lows.insert(delay.as_millis());
            }
        }
    }
    // At the cap the delay still spreads over [60, 120).
    assert!(lows.len() >= 5, "no jitter at the cap: {lows:?}");
    assert_eq!(c.min_delay(5), d(60));
    assert_eq!(c.max_delay(5), d(120));
}

#[test]
fn independent_draws_differ_between_hosts() {
    let c = Config::default();
    let a = c.draw(2, &mut Script::new(&[0.1]));
    let b = c.draw(2, &mut Script::new(&[0.8]));
    assert_ne!(a, b);
}

#[test]
fn the_system_rng_stays_in_the_unit_interval() {
    let mut r = SystemRng;
    for _ in 0..1000 {
        let u = r.unit();
        assert!((0.0..1.0).contains(&u));
    }
}

#[test]
fn a_captain_error_is_not_an_empty_answer() {
    let mut r = ready(exact());
    let mut rng = Script::new(&[0.0]);
    assert!(matches!(r.poll(t(0)), Poll::Request { .. }));
    r.on_answer(t(0), false, &mut rng);
    assert!(matches!(r.poll(t(30)), Poll::Request { .. }));
    r.on_answer(t(30), false, &mut rng);
    assert_eq!(r.streak(), 2);
    // Errors retry at the current delay and never move the streak.
    for i in 0..4 {
        let now = t(100 + 100 * i);
        assert!(matches!(r.poll(now), Poll::Request { .. }));
        r.on_error(now, &mut rng);
        assert_eq!(r.streak(), 2, "errors must not escalate backoff");
        assert_eq!(r.poll(now), Poll::Wait(d(60)));
    }
    // An error after work-reset stays at the initial delay.
    assert!(matches!(r.poll(t(1000)), Poll::Request { .. }));
    r.on_answer(t(1000), true, &mut rng);
    assert!(matches!(r.poll(t(1000)), Poll::Request { .. }));
    r.on_error(t(1000), &mut rng);
    assert_eq!(r.poll(t(1000)), Poll::Wait(d(30)));
}

#[test]
fn a_reload_keeps_the_streak_and_caps_the_pending_wait_at_the_new_max() {
    let mut r = ready(exact());
    let mut rng = Script::new(&[0.0]);
    for n in 0..3 {
        let now = t(1000 * n);
        assert!(matches!(r.poll(now), Poll::Request { .. }));
        r.on_answer(now, false, &mut rng);
    }
    // Scheduled 120 s out, from t=2000.
    assert_eq!(r.poll(t(2000)), Poll::Wait(d(120)));
    let shorter = Config {
        initial: d(5),
        max: d(20),
        ..exact()
    };
    r.set_config(shorter, t(2000));
    assert_eq!(r.streak(), 3);
    assert_eq!(r.poll(t(2000)), Poll::Wait(d(20)));
    assert!(matches!(r.poll(t(2020)), Poll::Request { .. }));
    r.on_answer(t(2020), false, &mut rng);
    assert_eq!(r.poll(t(2020)), Poll::Wait(d(20)), "the new max applies at once");
}

#[test]
fn config_defaults_and_validation() {
    assert_eq!(Config::from_effective(&json!({})).unwrap(), Config::default());
    let cfg = Config::from_effective(&json!({"fleetDispatch": {"request": {
        "initialBackoffSecs": 10, "maxBackoffSecs": 80, "jitter": 0.25, "startupSecs": 5
    }}}))
    .unwrap();
    assert_eq!(cfg.initial, d(10));
    assert_eq!(cfg.max, d(80));
    assert_eq!(cfg.min_delay(0), Duration::from_millis(7500));
    for (bad, why) in [
        (json!({"initialBackoffSecs": 0}), "zero"),
        (json!({"initialBackoffSecs": 60, "maxBackoffSecs": 30}), "cap below initial"),
        (json!({"jitter": 1.5}), "jitter over 1"),
        (json!({"jitter": -0.1}), "negative jitter"),
        (json!({"startupSecs": 500}), "startup over the cap"),
        (json!({"maxBackoffSecs": "soon"}), "not a number"),
        (json!({"initialBackoffSecs": -3}), "negative"),
    ] {
        let wrapped = json!({"fleetDispatch": {"request": bad}});
        assert!(Config::from_effective(&wrapped).is_err(), "{why} must be rejected");
    }
    let nan = Config {
        jitter: f64::NAN,
        ..Config::default()
    };
    assert!(nan.validate().is_err(), "a non-finite jitter is rejected");
}
