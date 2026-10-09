//! The output watchdog's feed (#10916 slice 3a): reader failures, staleness
//! and warm-up, and the alert tick it drives.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Duration as Cd, TimeZone, Utc};

use super::output_feed::{settle, source_at, OutputReader, Refresher, STALE_AFTER};
use super::outputs::OutputWatch;
use super::state::{AlertState, Kind, Transition};
use super::task::{inbox_payload, run_tick, AlertSink, TickContext};
use crate::fleet_outputs::observed::{Observed, Reading};
use crate::fleet_outputs::OutputSource;
use crate::observability::captain_gauges::store::Heartbeat;
use crate::types::{CapacityReport, DaemonStatusReport};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 0).unwrap()
}

fn stale_after() -> Cd {
    Cd::from_std(STALE_AFTER).unwrap()
}

/// A reader whose results the test scripts.
struct Scripted {
    signoz: Result<String, String>,
    heartbeat: Result<Option<Heartbeat>, String>,
    roster: Vec<String>,
}

impl OutputReader for Scripted {
    fn signoz(&mut self, _now: DateTime<Utc>) -> Result<String, String> {
        self.signoz.clone()
    }
    fn heartbeat(&mut self) -> Result<Option<Heartbeat>, String> {
        self.heartbeat.clone()
    }
    fn roster(&mut self) -> Vec<String> {
        self.roster.clone()
    }
}

fn ci_run_line(at: DateTime<Utc>) -> String {
    let ns = at.timestamp_nanos_opt().unwrap();
    format!(
        "{{\"kind\":\"ci.run\",\"repo\":\"\",\"issue\":0,\"first_ns\":{ns},\"last_ns\":{ns}}}\n"
    )
}

#[test]
fn settle_keeps_the_last_good_read_until_it_is_stale() {
    let mut last = None;
    assert_eq!(settle(Ok(1), &mut last, t0()), Ok(1));
    let soon = t0() + stale_after() - Cd::seconds(1);
    assert_eq!(settle(Err::<i32, _>("x".into()), &mut last, soon), Ok(1));
    let late = t0() + stale_after() + Cd::seconds(1);
    assert_eq!(settle(Err::<i32, _>("x".into()), &mut last, late), Err("x".into()));
    let mut never = None;
    assert_eq!(settle(Err::<i32, _>("y".into()), &mut never, t0()), Err("y".into()));
}

#[test]
fn refresher_turns_a_malformed_body_into_a_read_error() {
    let mut reader = Scripted {
        signoz: Ok("{\"kind\":\"ci.run\"}".into()),
        heartbeat: Ok(None),
        roster: vec!["org/a".into()],
    };
    let (reading, roster) = Refresher::default().read(&mut reader, t0());
    assert!(reading.signoz.is_err());
    assert_eq!(roster, vec!["org/a".to_string()]);
}

#[test]
fn refresher_rides_one_failure_then_fails_loud() {
    let mut reader = Scripted {
        signoz: Ok(ci_run_line(t0())),
        heartbeat: Ok(None),
        roster: Vec::new(),
    };
    let mut r = Refresher::default();
    assert!(r.read(&mut reader, t0()).0.signoz.is_ok());
    reader.signoz = Err("connection refused".into());
    reader.heartbeat = Err("forge 502".into());
    let (soon, _) = r.read(&mut reader, t0() + Cd::minutes(5));
    assert!(soon.signoz.is_ok());
    let (late, _) = r.read(&mut reader, t0() + stale_after() + Cd::minutes(1));
    assert_eq!(late.signoz.unwrap_err(), "connection refused");
    assert_eq!(late.heartbeat.unwrap_err(), "forge 502");
}

#[test]
fn source_at_warms_up_then_fires_when_nothing_was_read() {
    let none = BTreeSet::new();
    assert!(source_at(None, t0(), none.clone(), t0() + Cd::minutes(1)).is_none());
    let o = source_at(None, t0(), none, t0() + stale_after() + Cd::seconds(1)).unwrap();
    assert!(o.unreadable("eta.fit").unwrap().contains("no output read"));
    assert!(o.unreadable("captain-gauges/v1:stage-dwell").is_some());
}

#[test]
fn source_at_fires_on_a_stalled_reader() {
    let reading = Reading {
        at: t0(),
        signoz: Ok(Vec::new()),
        heartbeat: Ok(None),
    };
    let latest = (reading, vec!["org/a".to_string()]);
    let fresh = source_at(Some(&latest), t0(), BTreeSet::new(), t0() + Cd::minutes(1)).unwrap();
    assert!(fresh.unreadable("eta.fit").is_none());
    let late = t0() + stale_after() + Cd::seconds(1);
    let stalled = source_at(Some(&latest), t0(), BTreeSet::new(), late).unwrap();
    assert!(stalled.unreadable("eta.fit").unwrap().contains("stalled"));
    assert_eq!(stalled.roster, vec!["org/a".to_string()]);
}

type Log = Arc<Mutex<Vec<Transition>>>;

struct Capture(Log);
impl AlertSink for Capture {
    fn name(&self) -> &'static str {
        "capture"
    }
    fn deliver(&self, t: &Transition, _host: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(t.clone());
        Ok(())
    }
}

fn healthy_status() -> DaemonStatusReport {
    DaemonStatusReport {
        capacity: CapacityReport {
            ranking_present: true,
            total_accounts: 2,
            healthy_accounts: 2,
            exhausted_accounts: 0,
            token_axis_limit: 2,
            token_bound: false,
        },
        ..Default::default()
    }
}

/// A store read error reaches the sinks as a critical alert through the
/// production tick, and the thread survives to tick again.
#[test]
fn store_read_error_alerts_critical_through_run_tick() {
    let reading = Reading {
        at: t0(),
        signoz: Err("SigNoz HTTP 503".into()),
        heartbeat: Err("forge 502".into()),
    };
    let latest = (reading, vec!["org/a".to_string(), "org/b".to_string()]);
    let log: Log = Arc::default();
    let sinks: Vec<Box<dyn AlertSink>> = vec![Box::new(Capture(Arc::clone(&log)))];
    let mut state = AlertState::new(1, Duration::from_secs(6 * 3600));
    let status = healthy_status();
    for tick in 0..2 {
        let now = t0() + Cd::minutes(tick);
        let source: Observed = source_at(Some(&latest), t0(), BTreeSet::new(), now).unwrap();
        let watch = OutputWatch {
            source: &source,
            roster: &source.roster,
        };
        let ctx = TickContext {
            status: Some(&status),
            window: Duration::from_secs(1800),
            host: "captain",
            pool_dir: None,
            outputs: Some(&watch),
        };
        run_tick(&mut state, &sinks, &ctx, now);
    }
    let log = log.lock().unwrap();
    let started: Vec<&Transition> = log.iter().filter(|t| t.kind == Kind::Started).collect();
    assert!(started
        .iter()
        .any(|t| t.key == "output-missing:eta-authority:eta.estimate"));
    let estimate = started
        .iter()
        .find(|t| t.key.ends_with(":eta.estimate"))
        .unwrap();
    assert!(estimate.critical);
    assert!(
        estimate.headline.contains("CRITICAL") && estimate.headline.contains("SigNoz HTTP 503")
    );
    assert_eq!(inbox_payload(estimate, "captain")["severity"], "critical");
    // Debounced: one Started per key, no repeats on the second tick.
    let keys: BTreeSet<&str> = started.iter().map(|t| t.key.as_str()).collect();
    assert_eq!(keys.len(), started.len());
}

/// The module invariant (`fleet_alert/mod.rs`): no forge call anywhere in the
/// alert path, so a rate-limited `gh` can neither delay an alert nor make a
/// gauge row fire as unreadable (#10916).
#[test]
fn no_forge_call_in_output_feed() {
    let code: String = include_str!("output_feed.rs")
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for needle in [
        "fetch_heartbeat",
        "GhTransport",
        "fleet_store::gh",
        "hb_store",
        "Command::new",
    ] {
        assert!(
            !code.contains(needle),
            "output_feed.rs must make no forge call, but mentions `{needle}`"
        );
    }
}
