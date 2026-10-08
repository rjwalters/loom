//! The relay's bounded forward queue and its drain loop (Issue #10964).
//!
//! # Why this is not the daemon's own queue
//!
//! The daemon's records travel in a [`DurableQueue`]
//! (`observability::queue`): typed Loom envelopes, rewritten to disk whole on
//! every push, bounded by count and drop-oldest. An agent's OTLP batches are
//! far larger and far more frequent than those records. Sharing that queue
//! would let agent volume evict the daemon's own telemetry and would rewrite
//! megabytes per push, so the relay keeps **its own** queue per `otlp` sink
//! and drains it through that sink's own exporter — same endpoint, same
//! authentication and `headers_file`, same response policy
//! ([`super::super::transport`]) — on a separate task and HTTP client. A
//! stalled upstream therefore backs up only this queue.
//!
//! The cost of that choice, stated plainly: this queue is in memory. Relayed
//! telemetry still queued when the daemon stops is lost, where the daemon's
//! own records survive a restart.
//!
//! # Back-pressure
//!
//! [`RelayQueue::offer`] never waits. Past [`Limits::max_requests`] or
//! [`Limits::max_bytes`] the **oldest** queued request is discarded and its
//! records are counted against the session that sent them. The drain loop
//! turns those counts into one explicit `loom.agent_relay.gap` log record per
//! affected session — filed under that session's own bound resource, so it
//! sits beside the telemetry it describes — the next time the upstream
//! accepts anything. A consumer is told the stream is incomplete rather than
//! left to infer a complete one.
//!
//! [`DurableQueue`]: crate::observability::queue::DurableQueue

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs, SeverityNumber};
use opentelemetry_proto::tonic::resource::v1::Resource;

use super::super::mapping::{kv_string, nanos};
use super::super::transport::{post, Signal};
use super::super::OtlpExporter;
use super::sanitize::Payload;

/// `LogRecord.event_name` of a gap marker.
pub(super) const GAP_EVENT: &str = "loom.agent_relay.gap";

/// Why records were lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum GapReason {
    /// The queue was full: the upstream was slow or unreachable.
    QueueOverflow,
    /// The upstream answered with a non-retryable refusal.
    UpstreamRejected,
}

impl GapReason {
    fn as_str(self) -> &'static str {
        match self {
            GapReason::QueueOverflow => "relay_queue_overflow",
            GapReason::UpstreamRejected => "upstream_rejected",
        }
    }
}

/// Queue bounds.
#[derive(Debug, Clone, Copy)]
pub(super) struct Limits {
    /// Requests held at once.
    pub max_requests: usize,
    /// Sum of the queued requests' received body sizes.
    pub max_bytes: usize,
}

/// One sanitized request awaiting delivery.
#[derive(Debug, Clone)]
pub(in crate::observability::otlp) struct Batch {
    /// Already identity-bound and scrubbed.
    pub payload: Payload,
    /// Records it carries, in the signal's unit.
    pub items: u64,
    /// The size of the body it was decoded from — the queue's byte measure.
    pub bytes: usize,
    /// The sending session's secret-free label, for drop accounting.
    pub session: String,
    /// The sending session's bound resource, for its gap marker.
    pub resource: Resource,
}

/// Records lost for one session and reason, by signal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Lost {
    pub requests: u64,
    pub log_records: u64,
    pub metric_data_points: u64,
    pub spans: u64,
}

impl Lost {
    fn add(&mut self, signal: Signal, items: u64) {
        self.requests += 1;
        match signal {
            Signal::Logs => self.log_records += items,
            Signal::Metrics => self.metric_data_points += items,
            Signal::Traces => self.spans += items,
        }
    }

    fn merge(&mut self, other: &Lost) {
        self.requests += other.requests;
        self.log_records += other.log_records;
        self.metric_data_points += other.metric_data_points;
        self.spans += other.spans;
    }

    /// Every record lost, across signals.
    pub(super) fn records(&self) -> u64 {
        self.log_records + self.metric_data_points + self.spans
    }
}

/// Undelivered gap accounting: `(session label, reason)` → its resource and
/// what it lost.
type Gaps = BTreeMap<(String, GapReason), (Resource, Lost)>;

#[derive(Default)]
struct QueueState {
    /// Each request with the sequence number it was offered under, so the
    /// drain loop can tell whether the request it sent is still at the head.
    items: VecDeque<(u64, Arc<Batch>)>,
    next_sequence: u64,
    bytes: usize,
    gaps: Gaps,
    dropped_requests: u64,
    dropped_records: u64,
}

/// A bounded, in-memory, drop-oldest queue of sanitized requests.
pub(super) struct RelayQueue {
    limits: Limits,
    state: Mutex<QueueState>,
    wake: tokio::sync::Notify,
}

impl RelayQueue {
    pub(super) fn new(limits: Limits) -> Arc<Self> {
        Arc::new(RelayQueue {
            limits: Limits {
                max_requests: limits.max_requests.max(1),
                max_bytes: limits.max_bytes.max(1),
            },
            state: Mutex::new(QueueState::default()),
            wake: tokio::sync::Notify::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Enqueue `batch`, discarding the oldest queued requests first when a
    /// bound would be exceeded. Never waits on the upstream.
    pub(super) fn offer(&self, batch: Batch) {
        {
            let mut state = self.lock();
            while !state.items.is_empty()
                && (state.items.len() >= self.limits.max_requests
                    || state.bytes.saturating_add(batch.bytes) > self.limits.max_bytes)
            {
                if let Some((_, oldest)) = state.items.pop_front() {
                    state.bytes = state.bytes.saturating_sub(oldest.bytes);
                    note_lost(&mut state, &oldest, GapReason::QueueOverflow);
                }
            }
            state.bytes = state.bytes.saturating_add(batch.bytes);
            let sequence = state.next_sequence;
            state.next_sequence += 1;
            state.items.push_back((sequence, Arc::new(batch)));
        }
        self.wake.notify_one();
    }

    /// Requests queued now.
    pub(super) fn len(&self) -> usize {
        self.lock().items.len()
    }

    /// Requests discarded over this queue's lifetime, for any reason.
    pub(super) fn dropped_requests(&self) -> u64 {
        self.lock().dropped_requests
    }

    /// Records discarded over this queue's lifetime, for any reason.
    pub(super) fn dropped_records(&self) -> u64 {
        self.lock().dropped_records
    }

    fn front(&self) -> Option<(u64, Arc<Batch>)> {
        self.lock().items.front().cloned()
    }

    /// Whether any loss is waiting to be reported.
    fn has_gaps(&self) -> bool {
        !self.lock().gaps.is_empty()
    }

    /// Remove the front request if it is still the one that was sent
    /// (an overflow may have discarded it meanwhile). `lost` records it as a
    /// gap instead of a delivery.
    fn settle_front(&self, sent: u64, lost: Option<GapReason>) {
        let mut state = self.lock();
        if state.items.front().map(|(sequence, _)| *sequence) != Some(sent) {
            return;
        }
        if let Some((_, front)) = state.items.pop_front() {
            state.bytes = state.bytes.saturating_sub(front.bytes);
            if let Some(reason) = lost {
                note_lost(&mut state, &front, reason);
            }
        }
    }

    fn take_gaps(&self) -> Gaps {
        std::mem::take(&mut self.lock().gaps)
    }

    /// Put undelivered gap accounting back, merged with anything newer.
    fn restore_gaps(&self, gaps: Gaps) {
        let mut state = self.lock();
        for (key, (resource, lost)) in gaps {
            state
                .gaps
                .entry(key)
                .or_insert_with(|| (resource, Lost::default()))
                .1
                .merge(&lost);
        }
    }
}

fn note_lost(state: &mut QueueState, batch: &Batch, reason: GapReason) {
    state.dropped_requests += 1;
    state.dropped_records += batch.items;
    state
        .gaps
        .entry((batch.session.clone(), reason))
        .or_insert_with(|| (batch.resource.clone(), Lost::default()))
        .1
        .add(batch.payload.signal(), batch.items);
}

/// The gap markers for `gaps` as one logs request: a `WARN` record per
/// `(session, reason)`, each under that session's own bound resource.
pub(super) fn gap_request(gaps: &Gaps, now: chrono::DateTime<chrono::Utc>) -> Batch {
    let int = |key: &str, value: u64| KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::IntValue(i64::try_from(value).unwrap_or(i64::MAX))),
        }),
        ..Default::default()
    };
    let mut resource_logs = Vec::with_capacity(gaps.len());
    for ((_, reason), (resource, lost)) in gaps {
        let record = LogRecord {
            time_unix_nano: nanos(now),
            observed_time_unix_nano: nanos(now),
            severity_number: SeverityNumber::Warn as i32,
            severity_text: "WARN".to_string(),
            event_name: GAP_EVENT.to_string(),
            body: Some(AnyValue {
                value: Some(any_value::Value::StringValue(format!(
                    "agent telemetry relay dropped {} record(s) in {} request(s): {}",
                    lost.records(),
                    lost.requests,
                    reason.as_str()
                ))),
            }),
            attributes: vec![
                kv_string("loom.relay.gap_reason", reason.as_str()),
                int("loom.relay.dropped_requests", lost.requests),
                int("loom.relay.dropped_log_records", lost.log_records),
                int("loom.relay.dropped_metric_data_points", lost.metric_data_points),
                int("loom.relay.dropped_spans", lost.spans),
            ],
            ..Default::default()
        };
        resource_logs.push(ResourceLogs {
            resource: Some(resource.clone()),
            scope_logs: vec![ScopeLogs {
                log_records: vec![record],
                ..Default::default()
            }],
            ..Default::default()
        });
    }
    Batch {
        items: resource_logs.len() as u64,
        payload: Payload::Logs(ExportLogsServiceRequest { resource_logs }),
        bytes: 0,
        session: String::new(),
        resource: Resource::default(),
    }
}

/// What the upstream did with one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::observability::otlp) enum Delivery {
    /// Answered: nothing more to do with this request.
    Delivered,
    /// Not delivered, and worth sending again later.
    Retry,
    /// Refused in a way a resend cannot fix.
    Refused,
}

/// Where the drain loop sends requests. [`OtlpExporter`] in production; a
/// fake in the back-pressure tests.
pub(in crate::observability::otlp) trait Upstream:
    Send + Sync + 'static
{
    fn send(&self, batch: &Batch) -> impl std::future::Future<Output = Delivery> + Send;
}

impl Upstream for OtlpExporter {
    /// POST the request to this exporter's own signal endpoint, with its own
    /// authentication and `headers_file`, under the exporter's own response
    /// policy.
    async fn send(&self, batch: &Batch) -> Delivery {
        let signal = batch.payload.signal();
        let outcome = match &batch.payload {
            Payload::Logs(request) => {
                let endpoint = &self.logs_endpoint;
                post(
                    &self.client,
                    endpoint,
                    &self.ingest_key,
                    &self.extra_headers,
                    signal,
                    batch.items,
                    request,
                )
                .await
            }
            Payload::Metrics(request) => {
                let endpoint = &self.metrics_endpoint;
                post(
                    &self.client,
                    endpoint,
                    &self.ingest_key,
                    &self.extra_headers,
                    signal,
                    batch.items,
                    request,
                )
                .await
            }
            Payload::Traces(request) => {
                let endpoint = &self.traces_endpoint;
                post(
                    &self.client,
                    endpoint,
                    &self.ingest_key,
                    &self.extra_headers,
                    signal,
                    batch.items,
                    request,
                )
                .await
            }
        };
        if outcome.retry {
            Delivery::Retry
        } else if outcome.counts.dropped > 0 {
            // A non-retryable transport-level refusal (`transport::post`
            // counts the whole request as dropped). A partial rejection is
            // the receiver's own accounting and is not a relay gap.
            Delivery::Refused
        } else {
            Delivery::Delivered
        }
    }
}

/// First and largest pause after a retryable failure.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Drain `queue` into `upstream` until the task is dropped.
///
/// One request in flight at a time, in arrival order. A retryable failure
/// leaves the request at the head and backs off — while [`RelayQueue::offer`]
/// keeps accepting, and discarding the oldest, on its own.
pub(super) async fn drain<U: Upstream>(queue: Arc<RelayQueue>, upstream: U, backoff_min: Duration) {
    let mut backoff = backoff_min;
    let mut last_reported = 0u64;
    loop {
        // Tell the consumer about losses before sending anything newer, so a
        // gap is never reported after the records that followed it.
        let gaps = queue.take_gaps();
        if !gaps.is_empty() {
            let marker = gap_request(&gaps, chrono::Utc::now());
            match upstream.send(&marker).await {
                // A refused marker would be refused again; the loss stays in
                // the lifetime counters and the log line.
                Delivery::Delivered | Delivery::Refused => {}
                Delivery::Retry => {
                    queue.restore_gaps(gaps);
                    report(&queue, &mut last_reported);
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    continue;
                }
            }
        }
        let Some((sequence, batch)) = queue.front() else {
            if !queue.has_gaps() {
                queue.wake.notified().await;
            }
            continue;
        };
        match upstream.send(&batch).await {
            Delivery::Delivered => {
                queue.settle_front(sequence, None);
                backoff = backoff_min;
            }
            Delivery::Refused => {
                queue.settle_front(sequence, Some(GapReason::UpstreamRejected));
                backoff = backoff_min;
            }
            Delivery::Retry => {
                report(&queue, &mut last_reported);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
        report(&queue, &mut last_reported);
    }
}

/// Start [`drain`] with the production back-off.
pub(super) fn spawn_drain<U: Upstream>(
    queue: Arc<RelayQueue>,
    upstream: U,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(drain(queue, upstream, BACKOFF_MIN))
}

/// Log newly lost records — counts only, once per change.
fn report(queue: &RelayQueue, last_reported: &mut u64) {
    let dropped = queue.dropped_records();
    if dropped > *last_reported {
        log::warn!(
            "agent-relay: dropped {} relayed record(s) so far ({} request(s)); {} request(s) queued",
            dropped,
            queue.dropped_requests(),
            queue.len()
        );
        *last_reported = dropped;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "forward_tests.rs"]
mod tests;
