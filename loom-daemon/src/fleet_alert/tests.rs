#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;

use std::time::Duration;

use chrono::{DateTime, Duration as Cd, TimeZone, Utc};

use super::state::{self, AlertState, Kind};
use super::task::{inbox_payload, run_tick, AlertSink, BusSink};
use super::*;
use crate::types::{CapacityReport, DaemonStatusReport, RoleTickRecord, WorkFinderTickSummary};

const WINDOW: Duration = Duration::from_secs(30 * 60);

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 4, 12, 0, 0).unwrap()
}

fn status(healthy: usize, total: usize) -> DaemonStatusReport {
    DaemonStatusReport {
        capacity: CapacityReport {
            ranking_present: true,
            total_accounts: total,
            healthy_accounts: healthy,
            exhausted_accounts: total - healthy,
            token_axis_limit: healthy,
            token_bound: healthy == 0,
        },
        ..Default::default()
    }
}

fn failing_roles(s: &mut DaemonStatusReport, now: DateTime<Utc>, detail: &str) {
    for role in ["curator", "judge"] {
        for i in 0..3 {
            s.role_tick_records.push(RoleTickRecord {
                root: "/w/loom".into(),
                role: role.to_string(),
                at: now - Cd::minutes(10 - i),
                ok: false,
                detail: Some(detail.to_string()),
                pool_exhausted: false,
            });
        }
    }
}

type Log = Arc<Mutex<Vec<(Kind, String)>>>;

struct Fake {
    log: Log,
    fail: bool,
}
impl AlertSink for Fake {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn deliver(&self, t: &state::Transition, _host: &str) -> Result<(), String> {
        self.log.lock().unwrap().push((t.kind, t.key.clone()));
        if self.fail {
            Err("boom".into())
        } else {
            Ok(())
        }
    }
}

fn state() -> AlertState {
    AlertState::new(3, Duration::from_secs(6 * 3600))
}

#[test]
fn classify_healthy_is_empty() {
    assert!(classify(&status(5, 5), t0(), WINDOW, TokenCause::Exhausted).is_empty());
}

#[test]
fn classify_zero_healthy_names_auth_401_and_fix() {
    let c = classify(&status(0, 1), t0(), WINDOW, TokenCause::AuthDead);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].key, KEY_TOKENS);
    assert!(c[0].headline.contains("auth_401"));
    assert!(c[0].fix.contains("tokens unblock") && c[0].fix.contains("import-from-monitor"));
}

#[test]
fn classify_empty_pool_says_add_accounts() {
    let c = classify(&status(0, 0), t0(), WINDOW, TokenCause::EmptyPool);
    assert!(c[0].fix.contains("tokens bootstrap"));
}

#[test]
fn classify_halted_tick_without_token_starvation() {
    let mut s = status(3, 3);
    s.last_work_finder_tick = Some(WorkFinderTickSummary {
        halted: true,
        ..Default::default()
    });
    let c = classify(&s, t0(), WINDOW, TokenCause::Exhausted);
    assert_eq!(c.iter().map(|c| c.key).collect::<Vec<_>>(), vec![KEY_DISPATCH]);
    // With tokens at zero the halt is the token alert's symptom: not doubled.
    s.capacity.healthy_accounts = 0;
    let c = classify(&s, t0(), WINDOW, TokenCause::Exhausted);
    assert_eq!(c.iter().map(|c| c.key).collect::<Vec<_>>(), vec![KEY_TOKENS]);
}

#[test]
fn classify_persistent_roles_exit_78_guarded_refusal() {
    let mut s = status(3, 3);
    failing_roles(&mut s, t0(), "no live guarded-canary receipt (exit 78)");
    let c = classify(&s, t0(), WINDOW, TokenCause::Exhausted);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].key, KEY_ROLES);
    assert!(c[0].headline.contains("curator") && c[0].headline.contains("judge"));
    assert!(c[0].fix.contains("runtime/version mismatch"));
}

#[test]
fn step_debounce_started_reminder_cleared() {
    let mut st = state();
    let bad = classify(&status(0, 1), t0(), WINDOW, TokenCause::AuthDead);
    // Below debounce: nothing.
    assert!(st.step(t0(), &bad).is_empty());
    assert!(st.step(t0() + Cd::minutes(1), &bad).is_empty());
    let out = st.step(t0() + Cd::minutes(2), &bad);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].kind, Kind::Started);
    // Held for 24h at 1-minute ticks: 1 Started + floor(24h/6h) reminders.
    let mut alerts = 1;
    for m in 3..=(24 * 60 + 2) {
        alerts += st.step(t0() + Cd::minutes(m), &bad).len();
    }
    assert_eq!(alerts, 1 + 4);
    // Clears only after the debounce count of good ticks.
    let base = t0() + Cd::hours(25);
    assert!(st.step(base, &[]).is_empty());
    assert!(st.step(base + Cd::minutes(1), &[]).is_empty());
    let out = st.step(base + Cd::minutes(2), &[]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].kind, Kind::Cleared);
    assert!(st.step(base + Cd::minutes(3), &[]).is_empty());
}

#[test]
fn step_flap_below_debounce_sends_nothing() {
    let mut st = state();
    let bad = classify(&status(0, 1), t0(), WINDOW, TokenCause::AuthDead);
    for i in 0..10 {
        let now = t0() + Cd::minutes(i * 3);
        assert!(st.step(now, &bad).is_empty());
        assert!(st.step(now + Cd::minutes(1), &bad).is_empty());
        assert!(st.step(now + Cd::minutes(2), &[]).is_empty());
    }
}

#[test]
fn conditions_alert_and_clear_independently() {
    let mut st = state();
    let mut s = status(0, 1);
    failing_roles(&mut s, t0(), "failed");
    let both = classify(&s, t0(), WINDOW, TokenCause::AuthDead);
    assert_eq!(both.len(), 2);
    for i in 0..2 {
        assert!(st.step(t0() + Cd::minutes(i), &both).is_empty());
    }
    assert_eq!(st.step(t0() + Cd::minutes(2), &both).len(), 2);
    let only_roles: Vec<_> = both
        .iter()
        .filter(|c| c.key == KEY_ROLES)
        .cloned()
        .collect();
    let mut cleared = Vec::new();
    for i in 3..6 {
        cleared.extend(st.step(t0() + Cd::minutes(i), &only_roles));
    }
    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0].key, KEY_TOKENS);
    assert_eq!(cleared[0].kind, Kind::Cleared);
    assert!(st.is_active(KEY_ROLES));
}

#[test]
fn restart_with_persisted_state_does_not_repeat_started() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs").join("fleet-alert-state.json");
    let mut st = state();
    let bad = classify(&status(0, 1), t0(), WINDOW, TokenCause::AuthDead);
    for i in 0..3 {
        st.step(t0() + Cd::minutes(i), &bad);
    }
    st.save(&path);
    let mut reborn = AlertState::load(&path, 3, Duration::from_secs(6 * 3600));
    for i in 3..10 {
        assert!(reborn.step(t0() + Cd::minutes(i), &bad).is_empty());
    }
    // And it still clears after the restart.
    let mut out = Vec::new();
    for i in 10..13 {
        out.extend(reborn.step(t0() + Cd::minutes(i), &[]));
    }
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].kind, Kind::Cleared);
}

#[test]
fn a_failing_sink_does_not_suppress_the_other() {
    let log: Log = Arc::default();
    let sinks: Vec<Box<dyn AlertSink>> = vec![
        Box::new(Fake {
            log: log.clone(),
            fail: true,
        }),
        Box::new(Fake {
            log: log.clone(),
            fail: false,
        }),
    ];
    let mut st = AlertState::new(1, Duration::from_secs(3600));
    let out = run_tick(&mut st, &sinks, Some(&status(0, 1)), t0(), WINDOW, "h", None);
    assert_eq!(out.len(), 1);
    assert_eq!(log.lock().unwrap().len(), 2);
}

#[test]
fn unreachable_status_changes_nothing() {
    let log: Log = Arc::default();
    let sinks: Vec<Box<dyn AlertSink>> = vec![Box::new(Fake {
        log: log.clone(),
        fail: false,
    })];
    let mut st = AlertState::new(1, Duration::from_secs(3600));
    run_tick(&mut st, &sinks, Some(&status(0, 1)), t0(), WINDOW, "h", None);
    assert!(run_tick(&mut st, &sinks, None, t0(), WINDOW, "h", None).is_empty());
    assert!(st.is_active(KEY_TOKENS));
}

/// The 2026-10-04 incident: tokens 0/1 auth_401 plus persistent role failures.
/// No forge is involved anywhere in `run_tick`, so a rate-limited / absent
/// `gh` cannot affect delivery by construction.
#[test]
fn incident_2026_10_04_produces_alerts_naming_auth_401() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".ranking"),
        "agent15-2amlogic|blocked|0.00|2026-10-04T07:00:00Z\n",
    )
    .unwrap();
    let mut s = status(0, 1);
    failing_roles(&mut s, t0(), "spawn-worker exit 78: no live guarded-canary receipt");
    let bus = Arc::new(crate::event_bus::EventBus::new());
    let mut rx = bus.subscribe(["operator_priority"]);
    let sinks: Vec<Box<dyn AlertSink>> = vec![Box::new(BusSink(bus))];
    let mut st = AlertState::new(1, Duration::from_secs(3600));
    let out =
        run_tick(&mut st, &sinks, Some(&s), t0(), WINDOW, "joseph-superset", Some(dir.path()));
    assert_eq!(out.len(), 2);
    let tokens = out.iter().find(|t| t.key == KEY_TOKENS).unwrap();
    assert!(tokens.headline.contains("auth_401"));
    assert!(tokens.fix.contains("tokens unblock"));
    let ev = rx.try_recv().expect("bus event");
    assert_eq!(ev.topic(), "operator_priority.escalation");
    // Same state next tick: no second alert.
    assert!(
        run_tick(&mut st, &sinks, Some(&s), t0() + Cd::minutes(1), WINDOW, "h", None).is_empty()
    );
}

#[test]
fn inbox_payload_keyed_and_resolves_on_clear() {
    let t = state::Transition {
        kind: Kind::Started,
        key: KEY_TOKENS.into(),
        headline: "h".into(),
        fix: "f".into(),
    };
    let p = inbox_payload(&t, "box");
    assert_eq!(p["key"], "mail-box-fleet-degraded-tokens-zero-healthy");
    assert!(p["body"].as_str().unwrap().contains("Fix: f"));
    let c = state::Transition {
        kind: Kind::Cleared,
        ..t
    };
    let p = inbox_payload(&c, "box");
    assert_eq!(p["resolve"], true);
    assert_eq!(p["key"], "mail-box-fleet-degraded-tokens-zero-healthy");
}

#[test]
fn settings_block_and_defaults() {
    let d = Settings::from_block(None);
    assert!(!d.enabled);
    assert_eq!(d.debounce_ticks, 3);
    let s = Settings::from_block(Some(&serde_json::json!({
        "enabled": true, "reminderHours": 2, "debounceTicks": 5
    })));
    assert!(s.enabled);
    assert_eq!(s.reminder, Duration::from_secs(7200));
    assert_eq!(s.debounce_ticks, 5);
}

use std::sync::Arc;
