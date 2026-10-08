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
//! # What the byte bound measures
//!
//! A queued request is held **encoded** (protobuf), after binding and
//! scrubbing, so [`Limits::max_bytes`] bounds the memory the queue actually
//! holds — not the size of the bodies the requests arrived in, which can be
//! far smaller than what binding makes of them. Only the request at the head
//! is decoded again, by the drain loop, one at a time.
//!
//! Gap accounting is bounded too: past [`MAX_GAP_ENTRIES`] distinct
//! `(session, reason)` entries, further sessions' losses are folded into one
//! aggregate entry per reason, under a host-only resource, so an outage that
//! outlives many short sessions cannot grow it without limit. Totals are
//! never lost; only per-session attribution is, for the sessions past the cap.
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
use super::sanitize::{self, Encoding, Payload};
use prost::Message;

/// Distinct `(session, reason)` gap entries kept before further sessions are
/// folded into an aggregate.
pub(super) const MAX_GAP_ENTRIES: usize = 128;

/// The label losses past [`MAX_GAP_ENTRIES`] are folded under. Session labels
/// are sweep or role ids, which never contain a space.
pub(super) const OTHER_SESSIONS: &str = "other sessions";

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
    /// Sum of the queued requests' held sizes ([`Batch::bytes`]).
    pub max_bytes: usize,
}

/// One sanitized request awaiting delivery.
#[derive(Debug, Clone)]
pub(in crate::observability::otlp) struct Batch {
    signal: Signal,
    /// The identity-bound, scrubbed request, protobuf-encoded.
    encoded: Vec<u8>,
    /// Records it carries, in the signal's unit.
    pub items: u64,
    /// What holding it costs — the encoded request plus its gap resource and
    /// label. The queue's byte measure.
    pub bytes: usize,
    /// The sending session's secret-free label, for drop accounting.
    pub session: String,
    /// The sending session's bound resource, for its gap marker.
    pub resource: Resource,
}

impl Batch {
    /// Encode `payload` for holding.
    pub(in crate::observability::otlp) fn new(
        payload: &Payload,
        items: u64,
        session: String,
        resource: Resource,
    ) -> Self {
        let encoded = payload.encode_to_vec();
        let bytes = encoded
            .len()
            .saturating_add(resource.encoded_len())
            .saturating_add(session.len());
        Batch {
            signal: payload.signal(),
            encoded,
            items,
            bytes,
            session,
            resource,
        }
    }

    pub(in crate::observability::otlp) fn signal(&self) -> Signal {
        self.signal
    }

    /// The request, decoded again for sending. `None` only if the bytes this
    /// process encoded fail to decode, which would be a bug.
    pub(in crate::observability::otlp) fn payload(&self) -> Option<Payload> {
        sanitize::decode(self.signal, Encoding::Protobuf, &self.encoded).ok()
    }
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
        for ((label, reason), (resource, lost)) in gaps {
            fold(&mut state.gaps, &label, reason, &resource, &lost);
        }
    }

    /// Bytes held now, as [`Batch::bytes`] counts them.
    pub(super) fn held_bytes(&self) -> usize {
        self.lock().bytes
    }

    /// Gap entries waiting to be reported.
    pub(super) fn gap_entries(&self) -> usize {
        self.lock().gaps.len()
    }
}

fn note_lost(state: &mut QueueState, batch: &Batch, reason: GapReason) {
    state.dropped_requests += 1;
    state.dropped_records += batch.items;
    let mut lost = Lost::default();
    lost.add(batch.signal, batch.items);
    fold(&mut state.gaps, &batch.session, reason, &batch.resource, &lost);
}

/// Add `lost` to `label`'s entry, or — when that would be a new entry past
/// [`MAX_GAP_ENTRIES`] — to the aggregate entry for `reason`.
fn fold(gaps: &mut Gaps, label: &str, reason: GapReason, resource: &Resource, lost: &Lost) {
    let key = (label.to_string(), reason);
    let key = if gaps.contains_key(&key) || gaps.len() < MAX_GAP_ENTRIES {
        key
    } else {
        (OTHER_SESSIONS.to_string(), reason)
    };
    let aggregate = key.0 == OTHER_SESSIONS;
    gaps.entry(key)
        .or_insert_with(|| {
            let resource = if aggregate {
                host_only(resource)
            } else {
                resource.clone()
            };
            (resource, Lost::default())
        })
        .1
        .merge(lost);
}

/// `resource` without anything that names one session.
fn host_only(resource: &Resource) -> Resource {
    let keep = |key: &str| {
        matches!(
            key,
            "service.name"
                | "service.instance.id"
                | "loom.daemon.version"
                | "loom.runtime"
                | "loom.relay.redaction"
                | "loom.session.launch"
        ) || key.starts_with("host.")
    };
    Resource {
        attributes: resource
            .attributes
            .iter()
            .filter(|attribute| keep(&attribute.key))
            .cloned()
            .collect(),
        ..Default::default()
    }
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
    for ((label, reason), (resource, lost)) in gaps {
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
                KeyValue {
                    key: "loom.relay.aggregated".to_string(),
                    value: Some(AnyValue {
                        value: Some(any_value::Value::BoolValue(label == OTHER_SESSIONS)),
                    }),
                    ..Default::default()
                },
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
    let items = resource_logs.len() as u64;
    Batch::new(
        &Payload::Logs(ExportLogsServiceRequest { resource_logs }),
        items,
        String::new(),
        Resource::default(),
    )
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
        let signal = batch.signal();
        let Some(payload) = batch.payload() else {
            return Delivery::Refused;
        };
        let outcome = match &payload {
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
            "agent-relay: dropped {} relayed record(s) so far ({} request(s)); {} request(s) \
             ({} bytes) queued, {} gap entr(ies) awaiting report",
            dropped,
            queue.dropped_requests(),
            queue.len(),
            queue.held_bytes(),
            queue.gap_entries()
        );
        *last_reported = dropped;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "forward_tests.rs"]
mod tests;
