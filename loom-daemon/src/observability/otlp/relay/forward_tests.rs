//! Back-pressure in the relay's forward queue (#10964): bounded, drop-oldest,
//! counted, and reported — against upstreams that stall, fail and recover.
use super::*;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

fn resource(session: &str) -> Resource {
    Resource {
        attributes: vec![kv_string("loom.sweep_id", session)],
        ..Default::default()
    }
}

/// A logs request of `records` records from `session`, tagged `tag`.
fn logs(session: &str, tag: &str, records: usize, bytes: usize) -> Batch {
    let log_records = (0..records)
        .map(|_| LogRecord {
            event_name: tag.to_string(),
            ..Default::default()
        })
        .collect();
    let payload = Payload::Logs(ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(resource(session)),
            scope_logs: vec![ScopeLogs {
                log_records,
                ..Default::default()
            }],
            ..Default::default()
        }],
    });
    let mut batch = Batch::new(&payload, records as u64, session.to_string(), resource(session));
    // The tests set the held size directly, to exercise the byte bound.
    batch.bytes = bytes;
    batch
}

fn spans(session: &str, count: usize) -> Batch {
    let payload = Payload::Traces(ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            scope_spans: vec![ScopeSpans {
                spans: vec![Span::default(); count],
                ..Default::default()
            }],
            ..Default::default()
        }],
    });
    let mut batch = Batch::new(&payload, count as u64, session.to_string(), resource(session));
    batch.bytes = 10;
    batch
}

fn limits(max_requests: usize, max_bytes: usize) -> Limits {
    Limits {
        max_requests,
        max_bytes,
    }
}

/// The tag of a logs batch (its first record's event name).
fn tag(batch: &Batch) -> String {
    match &batch.payload().unwrap() {
        Payload::Logs(request) => request
            .resource_logs
            .first()
            .and_then(|r| r.scope_logs.first())
            .and_then(|s| s.log_records.first())
            .map(|record| record.event_name.clone())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

const DELIVER: u8 = 0;
const RETRY: u8 = 1;
const REFUSE: u8 = 2;
const STALL: u8 = 3;
/// Refuse only the request tagged `poison`; deliver everything else.
const REFUSE_POISON: u8 = 4;

/// An upstream whose behaviour the test switches while the drain runs.
#[derive(Clone)]
struct Fake {
    mode: Arc<AtomicU8>,
    sent: Arc<Mutex<Vec<Batch>>>,
    attempts: Arc<std::sync::atomic::AtomicU64>,
}

impl Fake {
    fn new(mode: u8) -> Self {
        Fake {
            mode: Arc::new(AtomicU8::new(mode)),
            sent: Arc::new(Mutex::new(Vec::new())),
            attempts: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    fn set(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
    }

    fn delivered(&self) -> Vec<Batch> {
        self.sent.lock().unwrap().clone()
    }
}

impl Upstream for Fake {
    async fn send(&self, batch: &Batch) -> Delivery {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        match self.mode.load(Ordering::SeqCst) {
            RETRY => Delivery::Retry,
            REFUSE => Delivery::Refused,
            REFUSE_POISON if tag(batch) == "poison" => Delivery::Refused,
            STALL => std::future::pending().await,
            _ => {
                self.sent.lock().unwrap().push(batch.clone());
                Delivery::Delivered
            }
        }
    }
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn start(queue: &Arc<RelayQueue>, upstream: &Fake) -> tokio::task::JoinHandle<()> {
    tokio::spawn(drain(queue.clone(), upstream.clone(), Duration::from_millis(2)))
}

#[test]
fn a_full_queue_drops_the_oldest_request_and_counts_what_it_held() {
    let queue = RelayQueue::new(limits(3, usize::MAX));
    for index in 0..5 {
        queue.offer(logs("sweep-a", &format!("r{index}"), 2, 10));
    }
    assert_eq!(queue.len(), 3);
    assert_eq!(queue.dropped_requests(), 2);
    assert_eq!(queue.dropped_records(), 4);
    // The survivors are the newest three, in order.
    let kept: Vec<String> = queue
        .lock()
        .items
        .iter()
        .map(|(_, batch)| tag(batch))
        .collect();
    assert_eq!(kept, ["r2", "r3", "r4"]);
}

#[test]
fn the_byte_bound_holds_too_and_one_oversized_request_is_still_accepted() {
    let queue = RelayQueue::new(limits(100, 100));
    queue.offer(logs("sweep-a", "a", 1, 60));
    queue.offer(logs("sweep-a", "b", 1, 60));
    assert_eq!(queue.len(), 1, "60 + 60 exceeds the 100-byte bound");
    assert_eq!(queue.lock().bytes, 60);
    // A request larger than the whole bound evicts everything and is kept:
    // refusing it would silently lose the newest data instead of the oldest.
    queue.offer(logs("sweep-a", "big", 1, 500));
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.dropped_requests(), 2);
}

#[test]
fn losses_are_accounted_per_session_and_per_signal() {
    let queue = RelayQueue::new(limits(1, usize::MAX));
    queue.offer(logs("sweep-a", "a", 3, 10));
    queue.offer(spans("sweep-a", 4));
    queue.offer(logs("sweep-b", "b", 5, 10));
    queue.offer(logs("sweep-b", "kept", 1, 10));
    let gaps = queue.take_gaps();
    let a = &gaps[&("sweep-a".to_string(), GapReason::QueueOverflow)].1;
    assert_eq!((a.requests, a.log_records, a.spans), (2, 3, 4));
    let b = &gaps[&("sweep-b".to_string(), GapReason::QueueOverflow)].1;
    assert_eq!((b.requests, b.log_records, b.spans), (1, 5, 0));
}

#[tokio::test]
async fn a_stalled_upstream_never_blocks_an_offer() {
    let queue = RelayQueue::new(limits(8, usize::MAX));
    let upstream = Fake::new(STALL);
    let task = start(&queue, &upstream);
    // Let the drain take the first request and hang on it forever.
    queue.offer(logs("sweep-a", "first", 1, 10));
    until("the stalled send to begin", || upstream.attempts.load(Ordering::SeqCst) > 0).await;
    let began = Instant::now();
    for index in 0..2_000 {
        queue.offer(logs("sweep-a", &format!("r{index}"), 1, 10));
    }
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "2000 offers against a hung upstream took {:?}",
        began.elapsed()
    );
    assert_eq!(queue.len(), 8, "the queue stays at its bound");
    assert_eq!(queue.dropped_requests(), 2_001 - 8);
    assert!(upstream.delivered().is_empty());
    task.abort();
}

#[tokio::test]
async fn an_outage_becomes_a_gap_marker_delivered_before_what_followed() {
    let queue = RelayQueue::new(limits(2, usize::MAX));
    let upstream = Fake::new(RETRY);
    let task = start(&queue, &upstream);
    for index in 0..6 {
        queue.offer(logs("sweep-a", &format!("r{index}"), 3, 10));
    }
    until("retries against the failing upstream", || {
        upstream.attempts.load(Ordering::SeqCst) >= 3
    })
    .await;
    assert!(upstream.delivered().is_empty());
    assert_eq!(queue.len(), 2, "a failing upstream leaves the queue at its bound");

    upstream.set(DELIVER);
    until("the backlog to drain", || queue.len() == 0).await;
    let delivered = upstream.delivered();
    // First the marker, then the two surviving requests, oldest first.
    assert_eq!(delivered.len(), 3, "{delivered:?}");
    assert_eq!(tag(&delivered[0]), GAP_EVENT);
    assert_eq!(tag(&delivered[1]), "r4");
    assert_eq!(tag(&delivered[2]), "r5");

    let Some(Payload::Logs(marker)) = delivered[0].payload() else {
        panic!("a gap marker is a log record")
    };
    assert_eq!(marker.resource_logs.len(), 1);
    // Filed under the losing session's own bound resource.
    assert_eq!(marker.resource_logs[0].resource, Some(resource("sweep-a")));
    let record = &marker.resource_logs[0].scope_logs[0].log_records[0];
    assert_eq!(record.severity_number, SeverityNumber::Warn as i32);
    let attribute = |key: &str| {
        record
            .attributes
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.clone())
    };
    assert_eq!(
        attribute("loom.relay.gap_reason"),
        Some(any_value::Value::StringValue("relay_queue_overflow".to_string()))
    );
    assert_eq!(attribute("loom.relay.dropped_requests"), Some(any_value::Value::IntValue(4)));
    assert_eq!(
        attribute("loom.relay.dropped_log_records"),
        Some(any_value::Value::IntValue(12))
    );
    assert_eq!(attribute("loom.relay.dropped_spans"), Some(any_value::Value::IntValue(0)));
    // Reported once: nothing is left to report again.
    assert!(queue.take_gaps().is_empty());
    assert_eq!(queue.dropped_records(), 12, "the lifetime counter is not reset by reporting");
    task.abort();
}

#[tokio::test]
async fn a_refused_request_is_dropped_counted_and_reported_not_retried_forever() {
    let queue = RelayQueue::new(limits(8, usize::MAX));
    let upstream = Fake::new(REFUSE_POISON);
    let task = start(&queue, &upstream);
    queue.offer(logs("sweep-a", "poison", 2, 10));
    until("the refused request to leave the queue", || queue.len() == 0).await;
    assert_eq!(queue.dropped_requests(), 1);
    assert_eq!(queue.dropped_records(), 2);
    // A poison request must not pin the head of the queue.
    queue.offer(logs("sweep-a", "next", 1, 10));
    until("the next request to be delivered", || {
        upstream
            .delivered()
            .iter()
            .any(|batch| tag(batch) == "next")
    })
    .await;
    let delivered = upstream.delivered();
    let marker = delivered
        .iter()
        .find(|batch| tag(batch) == GAP_EVENT)
        .expect("the refusal is reported");
    let Some(Payload::Logs(marker)) = marker.payload() else {
        panic!()
    };
    let record = &marker.resource_logs[0].scope_logs[0].log_records[0];
    assert!(record
        .attributes
        .iter()
        .any(|kv| kv.key == "loom.relay.gap_reason"
            && kv.value.as_ref().and_then(|v| v.value.clone())
                == Some(any_value::Value::StringValue("upstream_rejected".to_string()))));
    task.abort();
}

#[tokio::test]
async fn an_upstream_that_refuses_everything_cannot_grow_the_queue() {
    let queue = RelayQueue::new(limits(4, usize::MAX));
    let upstream = Fake::new(REFUSE);
    let task = start(&queue, &upstream);
    for index in 0..20 {
        queue.offer(logs("sweep-a", &format!("r{index}"), 1, 10));
    }
    until("every request to be dropped", || queue.len() == 0).await;
    assert_eq!(queue.dropped_requests(), 20);
    assert!(upstream.delivered().is_empty());
    task.abort();
}

#[test]
fn undelivered_gap_accounting_is_restored_and_merged() {
    let queue = RelayQueue::new(limits(1, usize::MAX));
    queue.offer(logs("sweep-a", "a", 2, 10));
    queue.offer(logs("sweep-a", "b", 1, 10));
    let taken = queue.take_gaps();
    // More is lost while the marker was in flight and failed.
    queue.offer(logs("sweep-a", "c", 1, 10));
    queue.restore_gaps(taken);
    let gaps = queue.take_gaps();
    let lost = &gaps[&("sweep-a".to_string(), GapReason::QueueOverflow)].1;
    assert_eq!((lost.requests, lost.log_records), (2, 3));
}

#[test]
fn settling_a_request_that_overflow_already_evicted_removes_nothing() {
    let queue = RelayQueue::new(limits(1, usize::MAX));
    queue.offer(logs("sweep-a", "sent", 1, 10));
    let (sequence, _) = queue.front().unwrap();
    // While it was in flight, it was evicted for a newer request.
    queue.offer(logs("sweep-a", "newer", 1, 10));
    queue.settle_front(sequence, None);
    assert_eq!(queue.len(), 1);
    assert_eq!(tag(&queue.front().unwrap().1), "newer");
}

#[test]
fn a_held_batch_is_measured_by_what_it_holds_and_round_trips() {
    let payload = Payload::Logs(ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(resource("sweep-a")),
            scope_logs: vec![ScopeLogs {
                log_records: vec![LogRecord {
                    event_name: "fixture".to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    });
    let batch = Batch::new(&payload, 1, "sweep-a".to_string(), resource("sweep-a"));
    assert_eq!(
        batch.bytes,
        payload.encoded_len() + resource("sweep-a").encoded_len() + "sweep-a".len()
    );
    assert_eq!(batch.payload(), Some(payload));
}

#[tokio::test]
async fn session_churn_through_an_outage_keeps_gap_state_bounded_and_every_loss_counted() {
    // Thousands of short sessions, each losing requests while the upstream
    // is down: the per-session map must stop growing at its cap, and the
    // aggregate must carry everything past it.
    let queue = RelayQueue::new(limits(4, usize::MAX));
    let upstream = Fake::new(RETRY);
    let task = start(&queue, &upstream);
    let sessions = 5_000;
    for index in 0..sessions {
        queue.offer(logs(&format!("sweep-{index}"), "r", 2, 10));
    }
    until("the drain to be retrying", || upstream.attempts.load(Ordering::SeqCst) >= 2).await;
    assert!(queue.gap_entries() <= MAX_GAP_ENTRIES, "{}", queue.gap_entries());
    let dropped = queue.dropped_records();
    assert_eq!(dropped, (sessions as u64 - 4) * 2);

    upstream.set(DELIVER);
    until("the backlog to drain", || queue.len() == 0 && queue.gap_entries() == 0).await;
    let delivered = upstream.delivered();
    let Some(Payload::Logs(marker)) = delivered
        .iter()
        .find(|batch| tag(batch) == GAP_EVENT)
        .and_then(Batch::payload)
    else {
        panic!("no gap marker delivered")
    };
    assert!(marker.resource_logs.len() <= MAX_GAP_ENTRIES);
    // Every lost record is accounted for, per session or in the aggregate.
    let mut reported = 0i64;
    let mut aggregated = 0;
    for resource_logs in &marker.resource_logs {
        let record = &resource_logs.scope_logs[0].log_records[0];
        for attribute in &record.attributes {
            match (attribute.key.as_str(), attribute.value.as_ref().and_then(|v| v.value.clone())) {
                ("loom.relay.dropped_log_records", Some(any_value::Value::IntValue(n))) => {
                    reported += n;
                }
                ("loom.relay.aggregated", Some(any_value::Value::BoolValue(true))) => {
                    aggregated += 1;
                    // The aggregate names no single session.
                    let resource = resource_logs.resource.as_ref().unwrap();
                    assert!(!resource
                        .attributes
                        .iter()
                        .any(|kv| kv.key == "loom.sweep_id"));
                }
                _ => {}
            }
        }
    }
    assert_eq!(reported as u64, dropped);
    assert_eq!(aggregated, 1);
    task.abort();
}
