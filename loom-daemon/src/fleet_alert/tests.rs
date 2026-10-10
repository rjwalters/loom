#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;

use std::time::Duration;

use chrono::{DateTime, Duration as Cd, TimeZone, Utc};

use super::state::{self, AlertState, Kind};
use super::task::{run_tick, AlertSink, BusSink, TickContext};
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
    assert_eq!(c.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), vec![KEY_DISPATCH]);
    // With tokens at zero the halt is the token alert's symptom: not doubled.
    s.capacity.healthy_accounts = 0;
    let c = classify(&s, t0(), WINDOW, TokenCause::Exhausted);
    assert_eq!(c.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), vec![KEY_TOKENS]);
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
    let out = run_tick(
        &mut st,
        &sinks,
        &TickContext {
            status: Some(&status(0, 1)),
            window: WINDOW,
            host: "h",
            pool_dir: None,
            outputs: None,
        },
        t0(),
    );
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
    run_tick(
        &mut st,
        &sinks,
        &TickContext {
            status: Some(&status(0, 1)),
            window: WINDOW,
            host: "h",
            pool_dir: None,
            outputs: None,
        },
        t0(),
    );
    assert!(run_tick(
        &mut st,
        &sinks,
        &TickContext {
            status: None,
            window: WINDOW,
            host: "h",
            pool_dir: None,
            outputs: None
        },
        t0()
    )
    .is_empty());
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
    let out = run_tick(
        &mut st,
        &sinks,
        &TickContext {
            status: Some(&s),
            window: WINDOW,
            host: "joseph-superset",
            pool_dir: Some(dir.path()),
            outputs: None,
        },
        t0(),
    );
    assert_eq!(out.len(), 2);
    let tokens = out.iter().find(|t| t.key == KEY_TOKENS).unwrap();
    assert!(tokens.headline.contains("auth_401"));
    assert!(tokens.fix.contains("tokens unblock"));
    let ev = rx.try_recv().expect("bus event");
    assert_eq!(ev.topic(), "operator_priority.escalation");
    // Same state next tick: no second alert.
    assert!(run_tick(
        &mut st,
        &sinks,
        &TickContext {
            status: Some(&s),
            window: WINDOW,
            host: "h",
            pool_dir: None,
            outputs: None
        },
        t0() + Cd::minutes(1)
    )
    .is_empty());
}

/// The common 401 path: `claude-wrapper.sh` / the `tokens check` reprobe write
/// an `auth-dead:` `.bad_tokens` entry, so the `blocked` row HAS history. It
/// must still classify as auth-dead (re-auth), not "wait for reset".
#[test]
fn blocked_with_auth_dead_bad_tokens_entry_is_auth_dead() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".ranking"),
        "agent15-2amlogic|blocked|0.00|2026-10-04T07:00:00Z\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join(".bad_tokens"),
        "2026-10-04T03:00:00Z agent15-2amlogic auth-dead: 401/invalid credential\n",
    )
    .unwrap();
    assert_eq!(causes::token_cause(1, Some(dir.path())), TokenCause::AuthDead);
}

/// A `blocked` row held by an exhaustion-class entry is a timed hold.
#[test]
fn blocked_with_exhaustion_bad_tokens_entry_is_exhausted() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".ranking"),
        "agent15-2amlogic|blocked|0.00|2026-10-04T07:00:00Z\n",
    )
    .unwrap();
    let ts = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    std::fs::write(
        dir.path().join(".bad_tokens"),
        format!("{ts} agent15-2amlogic exhausted: weekly limit reached\n"),
    )
    .unwrap();
    assert_eq!(causes::token_cause(1, Some(dir.path())), TokenCause::Exhausted);
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

// --- #10916 slice 2a: output watchdog riding the fleet alert path ---

mod output_watch {
    use std::collections::BTreeMap;

    use super::*;
    use crate::fleet_alert::outputs::{self, OutputWatch};
    use crate::fleet_outputs::OutputSource;

    /// Every registry row produced fresh, for every roster repo.
    struct Healthy(DateTime<Utc>);
    impl OutputSource for Healthy {
        fn last_seen(&self, _: &str) -> Option<DateTime<Utc>> {
            Some(self.0)
        }
        fn last_seen_per_repo(&self, _: &str) -> BTreeMap<String, DateTime<Utc>> {
            ["a/x", "b/y"]
                .iter()
                .map(|r| ((*r).to_string(), self.0))
                .collect()
        }
        fn expected_repos(&self, _: &str) -> Option<Vec<String>> {
            None
        }
    }

    /// Nothing observed at all (a host that does not produce).
    struct Silent;
    impl OutputSource for Silent {
        fn last_seen(&self, _: &str) -> Option<DateTime<Utc>> {
            None
        }
        fn last_seen_per_repo(&self, _: &str) -> BTreeMap<String, DateTime<Utc>> {
            BTreeMap::new()
        }
    }

    /// Everything fresh except one fleet-wide `record_kind`.
    struct SilentKind(&'static str, DateTime<Utc>);
    impl OutputSource for SilentKind {
        fn last_seen(&self, kind: &str) -> Option<DateTime<Utc>> {
            (kind != self.0).then_some(self.1)
        }
        fn last_seen_per_repo(&self, _: &str) -> BTreeMap<String, DateTime<Utc>> {
            ["a/x", "b/y"]
                .iter()
                .map(|r| ((*r).to_string(), self.1))
                .collect()
        }
    }

    /// `ci.run` is a Warning row (a quiet spell is legal): it must reach the
    /// inbox as a non-critical alert, while critical rows stay critical.
    #[test]
    fn warning_row_is_not_escalated_to_critical() {
        let r = roster();
        let src = SilentKind("ci.run", t0());
        let w = OutputWatch {
            source: &src,
            roster: &r,
        };
        let conds = outputs::conditions(&w, t0());
        assert_eq!(conds.len(), 1);
        assert!(conds[0].headline.starts_with("WARNING"));
        assert!(!conds[0].critical);

        let mut st = AlertState::new(1, Duration::from_secs(3600));
        let sinks: Vec<Box<dyn AlertSink>> = Vec::new();
        let out = run_tick(
            &mut st,
            &sinks,
            &TickContext {
                status: Some(&status(1, 1)),
                window: WINDOW,
                host: "h",
                pool_dir: None,
                outputs: Some(&w),
            },
            t0(),
        );
        assert_eq!(out.len(), 1);
        assert!(outputs::is_output_key(&out[0].key));
        assert!(!out[0].critical);
    }

    fn roster() -> Vec<String> {
        vec!["a/x".into(), "b/y".into()]
    }

    #[test]
    fn healthy_fleet_does_not_fire() {
        let r = roster();
        let src = Healthy(t0());
        let w = OutputWatch {
            source: &src,
            roster: &r,
        };
        assert!(outputs::conditions(&w, t0()).is_empty());
    }

    #[test]
    fn silent_fleet_fires_critical_through_the_sinks_after_debounce() {
        let r = roster();
        let w = OutputWatch {
            source: &Silent,
            roster: &r,
        };
        let conds = outputs::conditions(&w, t0());
        assert!(!conds.is_empty());
        assert!(conds.iter().all(|c| outputs::is_output_key(&c.key)));
        assert!(conds.iter().any(|c| c.headline.starts_with("CRITICAL")));

        let mut st = AlertState::new(1, Duration::from_secs(3600));
        let sinks: Vec<Box<dyn AlertSink>> = Vec::new();
        let out = run_tick(
            &mut st,
            &sinks,
            &TickContext {
                status: Some(&status(1, 1)),
                window: WINDOW,
                host: "h",
                pool_dir: None,
                outputs: Some(&w),
            },
            t0(),
        );
        assert_eq!(out.len(), conds.len());
        assert!(out.iter().all(|t| t.kind == Kind::Started));
        assert!(out.iter().any(|t| t.critical));

        // Output resumes: every alert clears.
        let src = Healthy(t0());
        let ok = OutputWatch {
            source: &src,
            roster: &r,
        };
        let out = run_tick(
            &mut st,
            &sinks,
            &TickContext {
                status: Some(&status(1, 1)),
                window: WINDOW,
                host: "h",
                pool_dir: None,
                outputs: Some(&ok),
            },
            t0(),
        );
        assert!(out.iter().all(|t| t.kind == Kind::Cleared));
        assert!(!out.is_empty());
    }
}
