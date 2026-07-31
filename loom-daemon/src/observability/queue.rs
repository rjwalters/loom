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
use std::sync::Mutex;

use crate::telemetry::TelemetryEnvelope;

/// Env var overriding the queue file path (test seam), mirroring
/// [`crate::sweep_outcomes::OUTCOMES_JOURNAL_PATH_ENV`].
pub const QUEUE_PATH_ENV: &str = "LOOM_OBSERVABILITY_QUEUE_PATH";

/// Default filename under `<workspace_root>/.loom/logs/`.
pub const QUEUE_FILENAME: &str = "observability-queue.jsonl";

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
        let mut state = self.lock();
        if state.items.len() >= self.capacity {
            state.items.pop_front();
            state.dropped_total += 1;
            log::warn!(
                "observability: queue at capacity ({}); dropped oldest record \
                 (dropped_total={})",
                self.capacity,
                state.dropped_total
            );
        }
        state.items.push_back(envelope);
        self.persist(&state.items);
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

    /// Remove the front `n` envelopes (clamped to the current length) after a
    /// successful export, persisting the drained state to disk.
    pub fn ack(&self, n: usize) {
        let mut state = self.lock();
        let n = n.min(state.items.len());
        state.items.drain(0..n);
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
            }),
        )
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
}
