//! Tests for the shared burn seam (Issues #8857, #8930).

use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};

use super::{
    burn_points, Burn, BurnEvent, BurnLedger, BurnSource, Emit, ModelBurn, WindowBurn,
    LATE_GRACE_SECS, MESSAGE_SETTLE_LAG_SECS,
};
use crate::telemetry::ops::{MetricName, MetricValue};

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_790_000_000 + secs, 0).unwrap()
}

fn event(secs: i64, input: i64) -> BurnEvent {
    BurnEvent {
        provider: "zai".into(),
        model: "glm".into(),
        at: at(secs),
        usage: ModelBurn {
            input,
            requests: 1,
            ..ModelBurn::default()
        },
    }
}

fn key() -> (String, String) {
    ("zai".to_string(), "glm".to_string())
}

#[test]
fn the_ledger_counts_events_through_the_window_end_and_holds_later_ones() {
    let mut ledger = BurnLedger::default();
    ledger.push([event(10, 1), event(60, 2), event(61, 4)]);
    let first = ledger.drain_through(at(60));
    assert_eq!(first[&key()].input, 3);
    assert_eq!(first[&key()].requests, 2);
    let second = ledger.drain_through(at(120));
    assert_eq!(second[&key()].input, 4, "the held event counts once, next window");
    assert!(ledger.drain_through(at(180)).is_empty());
}

#[test]
fn an_event_far_in_the_future_is_not_held_forever() {
    let mut ledger = BurnLedger::default();
    ledger.push([event(LATE_GRACE_SECS + 61, 5)]);
    assert_eq!(ledger.drain_through(at(60))[&key()].input, 5);
}

#[test]
fn burn_points_label_provider_and_model_and_skip_zeroes() {
    let burn: WindowBurn = [(
        ("claude".to_string(), "claude-opus".to_string()),
        ModelBurn {
            input: 5,
            output: 0,
            cache_read: 100,
            cache_write: 7,
            requests: 2,
        },
    )]
    .into_iter()
    .collect();
    let points = burn_points(&burn);
    let names: Vec<MetricName> = points.iter().map(|p| p.name).collect();
    assert_eq!(
        names,
        vec![
            MetricName::LlmTokensInput,
            MetricName::LlmTokensCacheRead,
            MetricName::LlmTokensCacheWrite,
            MetricName::LlmRequests,
        ]
    );
    for point in &points {
        assert_eq!(point.labels["provider"], "claude");
        assert_eq!(point.labels["model"], "claude-opus");
        assert!(!point.labels.contains_key("account"));
    }
    assert!(matches!(points[3].value, MetricValue::Int(2)));
}

/// A source that replays scripted events and records each poll's bounds.
#[derive(Clone, Default)]
struct Scripted {
    events: Arc<Mutex<Vec<BurnEvent>>>,
    polls: Arc<Mutex<Vec<(DateTime<Utc>, DateTime<Utc>)>>>,
}

impl BurnSource for Scripted {
    fn poll(&mut self, emit: Emit, out: &mut Vec<BurnEvent>) {
        self.polls.lock().unwrap().push((emit.not_before, emit.now));
        out.extend(
            self.events
                .lock()
                .unwrap()
                .drain(..)
                .filter(|e| emit.accepts(e.at)),
        );
    }
}

#[test]
fn the_first_sample_anchors_and_later_windows_abut() {
    let source = Scripted::default();
    let mut burn = Burn::new(vec![Box::new(source.clone())]);
    let lag = MESSAGE_SETTLE_LAG_SECS;
    // History before the anchor is never counted; usage after it is.
    source
        .events
        .lock()
        .unwrap()
        .extend([event(0, 100), event(1000 - lag + 1, 1)]);
    assert!(burn.sample(at(1000)).is_none());
    let (start, end, first) = burn.sample(at(1300)).unwrap();
    assert_eq!((start, end), (at(1000 - lag), at(1300 - lag)));
    assert_eq!(first[&key()].input, 1);
    let (next_start, _, second) = burn.sample(at(1600)).unwrap();
    assert_eq!(next_start, end);
    assert!(second.is_empty());
    // A clock that went backwards polls nothing and keeps the window.
    let polls = source.polls.lock().unwrap().len();
    assert!(burn.sample(at(1200)).is_none());
    assert_eq!(source.polls.lock().unwrap().len(), polls);
    assert_eq!(burn.sample(at(1900)).unwrap().0, at(1600 - lag));
}

#[test]
fn a_late_event_counts_in_the_current_window_and_history_never_does() {
    let source = Scripted::default();
    let mut burn = Burn::new(vec![Box::new(source.clone())]);
    burn.sample(at(0));
    burn.sample(at(3600));
    source.events.lock().unwrap().extend([
        // Written after its window closed: counted now, not lost.
        event(3000, 7),
        // Older than the grace: a copied or replayed record.
        event(3600 - 60 - LATE_GRACE_SECS - 1, 1000),
    ]);
    let (_, _, window) = burn.sample(at(3900)).unwrap();
    assert_eq!(window[&key()].input, 7);
}

#[test]
fn zero_usage_events_are_dropped() {
    let source = Scripted::default();
    let mut burn = Burn::new(vec![Box::new(source.clone())]);
    burn.sample(at(0));
    let mut empty = event(100, 0);
    empty.usage.requests = 0;
    source.events.lock().unwrap().push(empty);
    assert!(burn.sample(at(300)).unwrap().2.is_empty());
}
