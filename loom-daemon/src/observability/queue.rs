//! Durable, bounded, disk-backed offline queue for [`TelemetryEnvelope`]
//! records (Epic #4702, Phase 1 — issue #4705).
//!
//! Fleet hosts sleep and idle-shutdown (#4467/#4697), so an in-memory-only
//! queue would silently lose every unsent record across a host's downtime
//! window. This queue instead mirrors its in-memory state to a single JSONL
//! file (`<workspace>/.loom/logs/observability-queue.jsonl`, override via
//! [`QUEUE_PATH_ENV`]) on every mutation, using the same create-temp +
//! atomic-rename write pattern [`crate::idle_exit::write_marker`] and
//! [`crate::sweep_outcomes::append_outcome`] already use elsewhere in this
//! crate. [`DurableQueue::open`] replays that file at startup, so a record
//! enqueued just before a clean daemon exit (the idle-exit path never kills
//! `-9`) is still there to drain on the next boot.
//!
//! **Bounded, drop-oldest.** [`DurableQueue::push`] never grows the queue
//! past `capacity`: once full, the oldest queued envelope is discarded and
//! [`DurableQueue::dropped_total`] increments — logged once per drop, never a
//! silent loss and never unbounded growth (per the issue's AC).
//!
//! **Peek-then-ack, not pop.** The sender ([`super::sender`]) reads a batch
//! via [`DurableQueue::peek_batch`] without removing it, attempts the export,
//! and only calls [`DurableQueue::ack`] on success — a failed export leaves
//! the batch queued for the next retry rather than losing it.
//!
//! This whole-file rewrite on every mutation is `O(queue length)` per push/ack,
//! not an append-only journal — deliberately simple for Phase 1's expected
//! queue sizes (hundreds to a few thousand envelopes, per
//! `queueCapacity`'s default). A future phase can switch to an append+compact
//! journal (mirroring [`crate::sweep_outcomes`]'s rotation) if profiling ever
//! shows this matters.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::telemetry::TelemetryEnvelope;

/// Env var overriding the queue file path (test seam), mirroring
/// [`crate::sweep_outcomes::OUTCOMES_JOURNAL_PATH_ENV`].
pub const QUEUE_PATH_ENV: &str = "LOOM_OBSERVABILITY_QUEUE_PATH";

/// Default filename under `<workspace_root>/.loom/logs/`.
pub const QUEUE_FILENAME: &str = "observability-queue.jsonl";

/// Per-exporter queue filename under `<workspace_root>/.loom/logs/` when more
/// than one exporter is configured (Issue #8756) — `observability-queue.<name>
/// .jsonl` for the exporter named `name` ("https", "otlp"). A **sole**
/// exporter keeps [`QUEUE_FILENAME`] unchanged so a pre-fan-out backlog stays
/// discoverable without migration.
#[must_use]
pub fn named_queue_filename(name: &str) -> String {
    format!("observability-queue.{name}.jsonl")
}

/// Resolve the default queue path: [`QUEUE_PATH_ENV`] override (non-empty),
/// else `<workspace_root>/.loom/logs/observability-queue.jsonl`.
#[must_use]
pub fn default_queue_path(workspace_root: &Path) -> PathBuf {
    if let Ok(path) = std::env::var(QUEUE_PATH_ENV) {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    workspace_root
        .join(".loom")
        .join("logs")
        .join(QUEUE_FILENAME)
}

struct QueueState {
    items: VecDeque<TelemetryEnvelope>,
    dropped_total: u64,
    head_sequence: u128,
}

/// An in-process queue cursor plus immutable payload snapshot. Sequence identity
/// distinguishes even byte-identical envelopes pushed while an export is in flight.
pub struct QueueSnapshot {
    start_sequence: u128,
    pub envelopes: Vec<TelemetryEnvelope>,
}

/// A bounded, disk-backed FIFO queue of [`TelemetryEnvelope`]s.
pub struct DurableQueue {
    path: PathBuf,
    capacity: usize,
    state: Mutex<QueueState>,
}

impl DurableQueue {
    /// Open (or create) the queue at `path`, replaying any envelopes left
    /// over from a prior process (best-effort — a missing, unreadable, or
    /// partially-corrupt file degrades to an empty queue rather than
    /// erroring; a corrupt individual line is skipped, matching
    /// [`crate::sweep_outcomes::read_all`]'s soft-fail contract). `capacity`
    /// is clamped to at least 1.
    #[must_use]
    pub fn open(path: PathBuf, capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let mut items = load_existing(&path);
        // A capacity that shrank since the file was last written (e.g. a
        // config edit between restarts) must not resurrect more than the
        // new bound allows.
        while items.len() > capacity {
            items.pop_front();
        }
        DurableQueue {
            path,
            capacity,
            state: Mutex::new(QueueState {
                items,
                dropped_total: 0,
                head_sequence: 0,
            }),
        }
    }

    /// Enqueue `envelope`, dropping the oldest queued envelope first if the
    /// queue is already at `capacity`. Persists the new state to disk
    /// best-effort — a write failure is logged and swallowed (matches
    /// [`crate::sweep_outcomes::append_outcome`]'s "never block the caller on
    /// a journal-write failure" contract) so a full/unwritable disk degrades
    /// this queue to in-memory-only rather than crashing the collector.
    pub fn push(&self, envelope: TelemetryEnvelope) {
        if let Err(error) = self.push_inner(envelope, false) {
            log::warn!("observability: failed to persist queue: {error}");
        }
    }

    /// Offer a terminal span and confirm its queue file and directory reached
    /// stable storage before the caller removes active execution context.
    /// On failure the in-memory offer remains; retain context for recovery.
    pub fn push_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        self.push_inner(envelope, true)
    }

    fn push_inner(&self, envelope: TelemetryEnvelope, durable: bool) -> std::io::Result<()> {
        let mut state = self.lock();
        if state.items.len() >= self.capacity {
            state.items.pop_front();
            state.head_sequence += 1;
            state.dropped_total += 1;
            log::warn!(
                "observability: queue at capacity ({}); dropped oldest record \
                 (dropped_total={})",
                self.capacity,
                state.dropped_total
            );
        }
        state.items.push_back(envelope);
        persist_to(&self.path, &state.items)?;
        if durable {
            std::fs::File::open(&self.path)?.sync_all()?;
            let parent = self
                .path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    /// Current queue length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    /// True when the queue holds no envelopes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total envelopes dropped (oldest-first) over this queue's lifetime for
    /// having arrived while already at capacity.
    #[must_use]
    pub fn dropped_total(&self) -> u64 {
        self.lock().dropped_total
    }

    /// Clone up to `n` envelopes from the front of the queue **without**
    /// removing them — the sender only removes a batch after a confirmed
    /// successful export, via [`Self::ack`].
    #[must_use]
    pub fn peek_batch(&self, n: usize) -> Vec<TelemetryEnvelope> {
        self.lock().items.iter().take(n).cloned().collect()
    }

    /// Snapshot the payload and its sequence cursor under the same lock.
    #[must_use]
    pub fn peek_snapshot(&self, n: usize) -> QueueSnapshot {
        let state = self.lock();
        QueueSnapshot {
            start_sequence: state.head_sequence,
            envelopes: state.items.iter().take(n).cloned().collect(),
        }
    }

    /// Acknowledge only the exported snapshot prefix still present in the FIFO.
    /// Concurrent capacity drops may advance the head; newly pushed records must
    /// never be mistaken for records sent before that advance.
    pub fn ack_snapshot(&self, snapshot: &QueueSnapshot, acknowledged: usize) {
        let end = snapshot.start_sequence + acknowledged.min(snapshot.envelopes.len()) as u128;
        let mut state = self.lock();
        let count = end
            .saturating_sub(state.head_sequence)
            .min(state.items.len() as u128) as usize;
        state.items.drain(..count);
        state.head_sequence += count as u128;
        self.persist(&state.items);
    }

    /// Remove the front `n` envelopes (clamped to the current length) after a
    /// successful export, persisting the drained state to disk.
    pub fn ack(&self, n: usize) {
        let mut state = self.lock();
        let n = n.min(state.items.len());
        state.items.drain(0..n);
        state.head_sequence += n as u128;
        self.persist(&state.items);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn persist(&self, items: &VecDeque<TelemetryEnvelope>) {
        if let Err(error) = persist_to(&self.path, items) {
            log::warn!(
                "observability: failed to persist queue to {}: {error}",
                self.path.display()
            );
        }
    }
}

/// The collector/backfill/tracing push surface every envelope producer writes
/// through (Issue #8756). Before the multi-exporter fan-out each producer held
/// a `&DurableQueue`; now the same call sites write through this trait so the
/// single-sink and N-sink topologies share one producer path — `DurableQueue`
/// implements it directly (one sink) and [`FanoutQueue`] fans each offer out
/// to N per-exporter queues.
pub trait QueueSink: Send + Sync {
    /// Best-effort enqueue: a persistence failure is logged and swallowed by
    /// the implementation, never blocking the caller.
    fn offer(&self, envelope: TelemetryEnvelope);

    /// Durable enqueue for terminal records the caller may destroy its
    /// context for immediately after (trace spans): the queue file and its
    /// directory must reach stable storage before returning `Ok(())`.
    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()>;
}

impl QueueSink for DurableQueue {
    fn offer(&self, envelope: TelemetryEnvelope) {
        self.push(envelope);
    }

    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        self.push_durable(envelope)
    }
}

/// The multi-exporter fan point (Issue #8756): every offered envelope is
/// cloned into **each** per-exporter [`DurableQueue`], and each queue keeps a
/// fully independent drain/retry state — a stopped sink's backlog never holds
/// or re-sends a healthy sink's records, and vice versa.
pub struct FanoutQueue {
    queues: Vec<Arc<DurableQueue>>,
}

impl FanoutQueue {
    /// Fan offers out to `queues` in the given (config) order.
    #[must_use]
    pub fn new(queues: Vec<Arc<DurableQueue>>) -> Self {
        FanoutQueue { queues }
    }

    /// The per-exporter queues being fanned out to, in config order.
    #[must_use]
    pub fn queues(&self) -> &[Arc<DurableQueue>] {
        &self.queues
    }
}

impl QueueSink for FanoutQueue {
    fn offer(&self, envelope: TelemetryEnvelope) {
        for queue in &self.queues {
            queue.push(envelope.clone());
        }
    }

    /// Offers to every queue; the first persistence failure aborts the
    /// remaining fan-out and surfaces (`Err`) so the caller retains its
    /// recovery context — queues already offered keep their records, which is
    /// the same partial-write shape a single failing [`DurableQueue`] leaves.
    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        for queue in &self.queues {
            queue.push_durable(envelope.clone())?;
        }
        Ok(())
    }
}

fn load_existing(path: &Path) -> VecDeque<TelemetryEnvelope> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return VecDeque::new();
    };
    contents
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Atomically rewrite `path` to contain exactly `items`, one JSON object per
/// line — a temp-file-then-rename, mirroring
/// [`crate::idle_exit::write_marker`].
fn persist_to(path: &Path, items: &VecDeque<TelemetryEnvelope>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("jsonl.tmp-{}", std::process::id()));
    let mut buffer = String::new();
    for item in items {
        if let Ok(line) = serde_json::to_string(item) {
            buffer.push_str(&line);
            buffer.push('\n');
        }
    }
    std::fs::write(&temporary, buffer)?;
    std::fs::rename(temporary, path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::telemetry::{RepoVisibility, SweepStartedRecord, TelemetryRecord};
    use serial_test::serial;
    use tempfile::tempdir;

    fn envelope(issue: u32) -> TelemetryEnvelope {
        TelemetryEnvelope::new(
            "host-test",
            TelemetryRecord::SweepStarted(SweepStartedRecord {
                repo: "rjwalters/loom".to_string(),
                visibility: RepoVisibility::Public,
                issue,
                sweep_id: format!("sweep-issue-{issue}-0"),
                started_at: chrono::Utc::now(),
                model: None,
                effort: None,
                runtime: None,
            }),
        )
    }

    #[test]
    fn snapshot_ack_survives_capacity_eviction_and_identical_new_payloads() {
        let dir = tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 2);
        let first = envelope(1);
        let second = envelope(2);
        queue.push(first.clone());
        queue.push(second.clone());
        let snapshot = queue.peek_snapshot(2);
        queue.push(second.clone()); // evicts first; identical to an in-flight record
        queue.ack_snapshot(&snapshot, 2);
        assert_eq!(queue.peek_batch(2), vec![second.clone()]);
        queue.ack_snapshot(&snapshot, 2); // replayed acknowledgment is idempotent
        assert_eq!(queue.peek_batch(2), vec![second.clone()]);
        let snapshot = queue.peek_snapshot(1);
        queue.push(first.clone());
        queue.push(second.clone()); // evicts every record in snapshot
        queue.ack_snapshot(&snapshot, 1);
        assert_eq!(queue.peek_batch(2), vec![first, second]);
    }

    #[test]
    fn push_and_peek_round_trips_in_order() {
        let dir = tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 10);
        queue.push(envelope(1));
        queue.push(envelope(2));
        queue.push(envelope(3));
        assert_eq!(queue.len(), 3);
        let batch = queue.peek_batch(2);
        assert_eq!(batch.len(), 2);
        match &batch[0].record {
            TelemetryRecord::SweepStarted(r) => assert_eq!(r.issue, 1),
            other => panic!("unexpected record {other:?}"),
        }
        // peek does not remove.
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn ack_removes_only_the_front_n() {
        let dir = tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 10);
        for i in 1..=3 {
            queue.push(envelope(i));
        }
        queue.ack(2);
        assert_eq!(queue.len(), 1);
        let remaining = queue.peek_batch(10);
        match &remaining[0].record {
            TelemetryRecord::SweepStarted(r) => assert_eq!(r.issue, 3),
            other => panic!("unexpected record {other:?}"),
        }
    }

    #[test]
    fn ack_clamps_to_queue_length() {
        let dir = tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 10);
        queue.push(envelope(1));
        queue.ack(100); // must not panic
        assert!(queue.is_empty());
    }

    #[test]
    fn bounded_capacity_drops_oldest_and_counts_it() {
        let dir = tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 2);
        queue.push(envelope(1));
        queue.push(envelope(2));
        queue.push(envelope(3)); // over capacity: drops issue 1
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.dropped_total(), 1);
        let batch = queue.peek_batch(10);
        match &batch[0].record {
            TelemetryRecord::SweepStarted(r) => assert_eq!(r.issue, 2),
            other => panic!("unexpected record {other:?}"),
        }
        queue.push(envelope(4));
        queue.push(envelope(5));
        assert_eq!(queue.dropped_total(), 3, "never grows unbounded — every overflow is counted");
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn survives_reopen_across_a_simulated_restart() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        {
            let queue = DurableQueue::open(path.clone(), 10);
            queue.push(envelope(1));
            queue.push(envelope(2));
        }
        // Fresh queue instance over the same path — simulates a daemon
        // restart across host sleep/idle-shutdown (#4467/#4697).
        let reopened = DurableQueue::open(path, 10);
        assert_eq!(reopened.len(), 2);
        let batch = reopened.peek_batch(10);
        match &batch[0].record {
            TelemetryRecord::SweepStarted(r) => assert_eq!(r.issue, 1),
            other => panic!("unexpected record {other:?}"),
        }
    }

    #[test]
    fn missing_file_opens_as_an_empty_queue() {
        let dir = tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("does-not-exist.jsonl"), 10);
        assert!(queue.is_empty());
        assert_eq!(queue.dropped_total(), 0);
    }

    #[test]
    fn shrunk_capacity_on_reopen_drops_from_the_front() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        {
            let queue = DurableQueue::open(path.clone(), 10);
            for i in 1..=5 {
                queue.push(envelope(i));
            }
        }
        let reopened = DurableQueue::open(path, 2);
        assert_eq!(reopened.len(), 2);
        let batch = reopened.peek_batch(10);
        match &batch[0].record {
            TelemetryRecord::SweepStarted(r) => assert_eq!(r.issue, 4),
            other => panic!("unexpected record {other:?}"),
        }
    }

    #[test]
    #[serial]
    fn default_queue_path_env_override_wins() {
        std::env::set_var(QUEUE_PATH_ENV, "/tmp/custom-observability-queue.jsonl");
        let path = default_queue_path(Path::new("/repos/loom"));
        std::env::remove_var(QUEUE_PATH_ENV);
        assert_eq!(path, PathBuf::from("/tmp/custom-observability-queue.jsonl"));
    }

    #[test]
    #[serial]
    fn default_queue_path_falls_back_to_workspace_logs_dir() {
        std::env::remove_var(QUEUE_PATH_ENV);
        let path = default_queue_path(Path::new("/repos/loom"));
        assert_eq!(path, PathBuf::from("/repos/loom/.loom/logs/observability-queue.jsonl"));
    }

    // ------------------------------------------------------------------
    // FanoutQueue (#8756) — the collector-facing fan point. One offered
    // envelope lands in EVERY per-exporter queue; each queue keeps an
    // independent drain state, so one sink's backlog/retry never holds or
    // re-sends another's.
    // ------------------------------------------------------------------

    #[test]
    fn fanout_offer_lands_in_every_queue_file() {
        let dir = tempdir().unwrap();
        let https = Arc::new(DurableQueue::open(dir.path().join("q.https.jsonl"), 10));
        let otlp = Arc::new(DurableQueue::open(dir.path().join("q.otlp.jsonl"), 10));
        let fanout = FanoutQueue::new(vec![https.clone(), otlp.clone()]);
        QueueSink::offer(&fanout, envelope(7));
        assert_eq!(https.len(), 1, "https sink received the envelope");
        assert_eq!(otlp.len(), 1, "otlp sink received the same envelope");
        assert!(dir.path().join("q.https.jsonl").exists());
        assert!(dir.path().join("q.otlp.jsonl").exists());
    }

    #[test]
    fn fanout_drain_states_are_independent() {
        let dir = tempdir().unwrap();
        let https = Arc::new(DurableQueue::open(dir.path().join("q.https.jsonl"), 10));
        let otlp = Arc::new(DurableQueue::open(dir.path().join("q.otlp.jsonl"), 10));
        let fanout = FanoutQueue::new(vec![https.clone(), otlp.clone()]);
        QueueSink::offer(&fanout, envelope(1));
        QueueSink::offer(&fanout, envelope(2));
        // The https sender drains both; the otlp sink is down and drains none.
        https.ack(2);
        assert!(https.is_empty());
        assert_eq!(otlp.len(), 2, "a stopped sink's backlog is retained");
        // On recovery the otlp sender drains its own queue — and only its own.
        otlp.ack(1);
        assert_eq!(otlp.len(), 1, "no cross-sink re-send: https already acked");
    }

    #[test]
    fn fanout_offer_durable_propagates_sync_failures() {
        let dir = tempdir().unwrap();
        let ok = Arc::new(DurableQueue::open(dir.path().join("q.https.jsonl"), 10));
        // A queue whose backing file cannot be persisted: capacity-1 queue
        // backed by a path whose parent does not exist yet still accepts
        // in-memory — so instead use a directory as the file path to force
        // the persist to fail.
        let bad_path = dir.path().join("not-a-file");
        std::fs::create_dir_all(&bad_path).unwrap();
        let bad = Arc::new(DurableQueue::open(bad_path, 10));
        let fanout = FanoutQueue::new(vec![ok.clone(), bad.clone()]);
        assert!(
            QueueSink::offer_durable(&fanout, envelope(1)).is_err(),
            "a persist failure must surface to the caller, not be swallowed"
        );
    }

    #[test]
    fn fanout_empty_list_offers_nowhere() {
        let fanout = FanoutQueue::new(Vec::new());
        QueueSink::offer(&fanout, envelope(1)); // must not panic
        assert!(fanout.queues().is_empty());
    }
}
