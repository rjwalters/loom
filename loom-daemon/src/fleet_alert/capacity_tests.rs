//! #10214: the host-level capacity asks.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{Duration as Cd, TimeZone, Utc};

use super::capacity::{KEY_CAPACITY_LIMITED, KEY_STAR_BACKLOG};
use super::state::{AlertState, Kind, Transition};
use super::task::{inbox_payload, run_tick, AlertSink};
use super::{classify, TokenCause};
use crate::types::{
    CapView, CapacityReport, DaemonStatusReport, QueueDisposition, ReadyQueueRow,
    WorkFinderTickSummary,
};

const WINDOW: Duration = Duration::from_secs(30 * 60);

fn star(rank: usize, issue: u32) -> ReadyQueueRow {
    let d = QueueDisposition::DeferredCapacity;
    ReadyQueueRow {
        rank,
        repo: "/w/loom-ui".into(),
        issue,
        workspace_priority: 100,
        urgent: false,
        operator_priority: true,
        operator_priority_at: Some(format!("2026-10-0{}T00:00:00Z", 1 + rank.min(3))),
        main_red_fix: false,
        created_at: None,
        tier: None,
        story_points: None,
        disposition: d,
        detail: None,
        state: d.state().into(),
        reason: d.reason().into(),
        plan: crate::types::RowPlan::default(),
    }
}

/// Healthy tokens, a tick under `cap`, with `stars` starred issues waiting.
fn status(cap: CapView, stars: u32) -> DaemonStatusReport {
    DaemonStatusReport {
        capacity: CapacityReport {
            ranking_present: true,
            total_accounts: 3,
            healthy_accounts: 3,
            token_axis_limit: 3,
            ..Default::default()
        },
        last_work_finder_tick: Some(WorkFinderTickSummary {
            max_concurrent: cap.effective(),
            cap: Some(cap),
            queue: (0..stars).map(|i| star(i as usize + 1, 400 + i)).collect(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn disk_limited() -> CapView {
    CapView {
        configured: 6,
        disk: Some(2),
        ram: Some(30),
    }
}

/// An inbox stand-in: the only sink, as on a host with Safehouse disabled.
struct Inbox(Arc<Mutex<Vec<serde_json::Value>>>);
impl AlertSink for Inbox {
    fn name(&self) -> &'static str {
        "inbox"
    }
    fn deliver(&self, t: &Transition, host: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(inbox_payload(t, host));
        Ok(())
    }
}

fn t0() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 4, 4, 0, 0).unwrap()
}

#[test]
fn a_disk_limited_cap_and_a_starred_backlog_each_name_the_numbers() {
    let c = classify(&status(disk_limited(), 106), t0(), WINDOW, TokenCause::Exhausted);
    let keys: Vec<_> = c.iter().map(|c| c.key).collect();
    assert_eq!(keys, vec![KEY_CAPACITY_LIMITED, KEY_STAR_BACKLOG]);
    let cap = &c[0];
    assert!(cap.headline.contains("Disk headroom"), "{}", cap.headline);
    assert!(cap
        .headline
        .contains("capping concurrency at 2, below the configured 6"));
    assert!(cap.headline.contains("106 starred issue(s) are waiting"));
    assert!(cap.fix.contains("Free disk"));
    let backlog = &c[1];
    assert!(backlog
        .headline
        .contains("106 starred issues are waiting for 2 slot(s)"));
    assert!(backlog.headline.contains("position 107"), "{}", backlog.headline);
    assert!(backlog.headline.contains("#400 in /w/loom-ui"), "{}", backlog.headline);
    assert!(backlog.fix.contains("Unstar or re-rank"));
}

#[test]
fn a_configured_cap_with_a_short_queue_asks_nothing() {
    let configured = CapView {
        configured: 4,
        disk: Some(40),
        ram: None,
    };
    assert!(classify(&status(configured, 12), t0(), WINDOW, TokenCause::Exhausted).is_empty());
    // A disk-limited cap is an ask even with nothing starred.
    let c = classify(&status(disk_limited(), 0), t0(), WINDOW, TokenCause::Exhausted);
    assert_eq!(c.iter().map(|c| c.key).collect::<Vec<_>>(), vec![KEY_CAPACITY_LIMITED]);
    assert!(!c[0].headline.contains("starred"));
}

#[test]
fn each_ask_reaches_the_inbox_exactly_once_without_safehouse() {
    let inbox = Arc::new(Mutex::new(Vec::new()));
    let sinks: Vec<Box<dyn AlertSink>> = vec![Box::new(Inbox(inbox.clone()))];
    let mut st = AlertState::new(3, Duration::from_secs(6 * 3600));
    // Three hours at 1-minute ticks while the backlog shifts between 100 and
    // 110 stars: the headline moves, the asks do not repeat.
    for m in 0..180 {
        let s = status(disk_limited(), 100 + (m % 11));
        run_tick(
            &mut st,
            &sinks,
            Some(&s),
            t0() + Cd::minutes(i64::from(m)),
            WINDOW,
            "worker-1",
            None,
        );
    }
    let sent = inbox.lock().unwrap().clone();
    let keys: Vec<&str> = sent.iter().map(|p| p["key"].as_str().unwrap()).collect();
    assert_eq!(
        keys,
        vec![
            "mail-worker-1-fleet-degraded-capacity-limited",
            "mail-worker-1-fleet-degraded-star-backlog",
        ]
    );
    // Freeing disk clears the capacity ask (and, with 6 slots, the backlog).
    let freed = CapView {
        disk: Some(20),
        ..disk_limited()
    };
    let mut cleared = Vec::new();
    for m in 180..184 {
        cleared.extend(run_tick(
            &mut st,
            &sinks,
            Some(&status(freed, 12)),
            t0() + Cd::minutes(m),
            WINDOW,
            "worker-1",
            None,
        ));
    }
    assert_eq!(cleared.len(), 2);
    assert!(cleared.iter().all(|t| t.kind == Kind::Cleared));
}
