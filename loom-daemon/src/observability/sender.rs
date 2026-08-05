//! Queue-drain sender loop with jittered retry/backoff (Epic #4702, Phase 1
//! — issue #4705).
//!
//! Generic over `E: `[`Exporter`] so a future OTLP exporter (epic Phase 4)
//! plugs into this same loop unchanged. Every `flush_interval` (jittered) the
//! loop drains as many full batches as are currently queued; a failed export
//! backs off exponentially (also jittered) before the next attempt, capped at
//! [`MAX_BACKOFF`], and resets to [`MIN_BACKOFF`] on the next success. Jitter
//! uses [`crate::tokens_pool::rng::Rng`] — this crate's existing dependency-
//! free PRNG (`tokens_pool::rng`'s doc: "none [external `rand` crate] exists
//! anywhere in this workspace's `Cargo.toml` today") rather than adding a
//! `rand` dependency.

use std::sync::Arc;
use std::time::Duration;

use crate::tokens_pool::rng::Rng;

use super::exporter::Exporter;
use super::queue::DurableQueue;
use super::ExportStatus;

/// Starting backoff after a failed export attempt.
pub const MIN_BACKOFF: Duration = Duration::from_secs(5);
/// Backoff ceiling — an unreachable sink never waits longer than this
/// between retries.
pub const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// Outcome of one [`try_flush`] attempt, for the caller's retry/backoff
/// decision.
#[derive(Debug, PartialEq, Eq)]
pub enum FlushOutcome {
    /// The queue was empty — nothing to send.
    Empty,
    /// A batch of this many envelopes was exported and acked.
    Sent(usize),
    /// The export attempt failed; the batch remains queued for retry.
    Failed,
}

/// Attempt to send one batch (up to `batch_size` envelopes, peeked from the
/// front of `queue`) via `exporter`. Only acks (removes) the batch from
/// `queue` on a confirmed successful export — a failure leaves the queue
/// untouched so the same envelopes are retried next time.
///
/// Every decided attempt (sent or failed) is also recorded on `status` (Issue
/// #5083) — this is the single point in the daemon that *knows* whether
/// telemetry is landing, so it is the only honest source for the positive
/// signal `loom-daemon status`/`health` report. An `Empty` queue records
/// nothing: nothing was attempted, so it is evidence of neither health nor
/// failure (this is exactly the ambiguity a 0-byte queue file left an operator
/// with before #5083).
pub async fn try_flush<E: Exporter>(
    queue: &DurableQueue,
    exporter: &E,
    batch_size: usize,
    status: &ExportStatus,
) -> FlushOutcome {
    let batch = queue.peek_batch(batch_size);
    if batch.is_empty() {
        return FlushOutcome::Empty;
    }
    match exporter.emit_batch(&batch).await {
        Ok(()) => {
            let sent = batch.len();
            queue.ack(sent);
            status.record_success(sent);
            FlushOutcome::Sent(sent)
        }
        Err(error) => {
            log::warn!(
                "observability: export failed, {} record(s) remain queued \
                 (dropped_total={}): {error}",
                queue.len(),
                queue.dropped_total()
            );
            status.record_failure(&error.to_string());
            FlushOutcome::Failed
        }
    }
}

/// Spawn the sender loop on the shared daemon runtime.
pub fn spawn_task<E>(
    queue: Arc<DurableQueue>,
    exporter: E,
    batch_size: usize,
    flush_interval: Duration,
    status: Arc<ExportStatus>,
) -> tokio::task::JoinHandle<()>
where
    E: Exporter + Send + Sync + 'static,
{
    tokio::spawn(run_sender(queue, exporter, batch_size, flush_interval, status))
}

async fn run_sender<E: Exporter>(
    queue: Arc<DurableQueue>,
    exporter: E,
    batch_size: usize,
    flush_interval: Duration,
    status: Arc<ExportStatus>,
) {
    let mut rng = Rng::from_entropy();
    let mut backoff = MIN_BACKOFF;
    loop {
        tokio::time::sleep(jittered(flush_interval, &mut rng)).await;
        // Drain every currently-queued batch before sleeping again, so a
        // burst that arrived between ticks does not wait a full extra
        // `flush_interval` per batch.
        loop {
            match try_flush(&queue, &exporter, batch_size, &status).await {
                FlushOutcome::Empty => {
                    backoff = MIN_BACKOFF;
                    break;
                }
                FlushOutcome::Sent(_) => {
                    backoff = MIN_BACKOFF;
                    // Loop again immediately — more may still be queued.
                }
                FlushOutcome::Failed => {
                    tokio::time::sleep(jittered(backoff, &mut rng)).await;
                    backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
                    break;
                }
            }
        }
    }
}

/// Apply +/-20% jitter to `base`, floored at zero. A `base` of zero (or
/// small enough that the jitter window rounds to zero) returns `base`
/// unchanged rather than dividing by zero.
fn jittered(base: Duration, rng: &mut Rng) -> Duration {
    let base_ms = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
    let window_ms = base_ms / 5; // 20%
    if window_ms == 0 {
        return base;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let offset_ms = rng.gen_range((window_ms * 2 + 1) as usize) as i64 - window_ms as i64;
    let jittered_ms = (i64::try_from(base_ms).unwrap_or(i64::MAX) + offset_ms).max(0);
    Duration::from_millis(u64::try_from(jittered_ms).unwrap_or(0))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::observability::exporter::ExportError;
    use crate::telemetry::{HostHealthRecord, TelemetryEnvelope, TelemetryRecord};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn envelope() -> TelemetryEnvelope {
        TelemetryEnvelope::new(
            "host-test",
            TelemetryRecord::HostHealth(HostHealthRecord {
                captured_at: chrono::Utc::now(),
                daemon_version: "0.16.0".to_string(),
                build_commit: "deadbeef".to_string(),
                built_at: None,
                uptime_sec: 1,
                logical_cpus: 4,
                cpu_idle_fraction: None,
                load_per_core: None,
                worktree_root_free_gb: None,
                worktree_root_total_gb: None,
                active_sweep_ids: Vec::new(),
                dispatch_halted: false,
                halt_reason: None,
                managed_repos: Vec::new(),
                roles: crate::telemetry::RoleTickHealth::default(),
            }),
        )
    }

    /// A toggleable in-memory exporter: [`FakeExporter::set_up`] flips
    /// between "sink reachable" and "sink down" without any real network I/O
    /// — this is the sender-loop-level analogue of `exporter::tests::
    /// MockSink`'s kill/revive, exercised at the [`try_flush`] granularity.
    struct FakeExporter {
        up: AtomicBool,
        received: Mutex<Vec<crate::telemetry::TelemetryEnvelope>>,
        call_count: AtomicUsize,
    }

    impl FakeExporter {
        fn new(up: bool) -> Self {
            FakeExporter {
                up: AtomicBool::new(up),
                received: Mutex::new(Vec::new()),
                call_count: AtomicUsize::new(0),
            }
        }

        fn set_up(&self, up: bool) {
            self.up.store(up, Ordering::SeqCst);
        }
    }

    impl Exporter for FakeExporter {
        async fn emit_batch(&self, envelopes: &[TelemetryEnvelope]) -> Result<(), ExportError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if !self.up.load(Ordering::SeqCst) {
                return Err(ExportError::Transport("sink down".to_string()));
            }
            self.received.lock().unwrap().extend_from_slice(envelopes);
            Ok(())
        }
    }

    /// A status cell for a just-started exporter, matching what
    /// [`super::spawn_task`] constructs in production.
    fn status_cell() -> ExportStatus {
        ExportStatus::started("host-test", "https://example.invalid/ingest", "https", 30)
    }

    #[tokio::test]
    async fn try_flush_on_empty_queue_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 10);
        let exporter = FakeExporter::new(true);
        let status = status_cell();
        let outcome = try_flush(&queue, &exporter, 10, &status).await;
        assert_eq!(outcome, FlushOutcome::Empty);
        // #5083: an empty queue is evidence of NEITHER health nor failure —
        // nothing was attempted. Recording a success here is exactly how a
        // never-exporting host would masquerade as healthy.
        let snapshot = status.snapshot();
        assert!(snapshot.last_success_at.is_none());
        assert_eq!(snapshot.consecutive_failures, 0);
        assert_eq!(snapshot.records_exported, 0);
    }

    #[tokio::test]
    async fn try_flush_sends_and_acks_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 10);
        queue.push(envelope());
        queue.push(envelope());
        let exporter = FakeExporter::new(true);
        let status = status_cell();
        let outcome = try_flush(&queue, &exporter, 10, &status).await;
        assert_eq!(outcome, FlushOutcome::Sent(2));
        assert!(queue.is_empty());
        assert_eq!(exporter.received.lock().unwrap().len(), 2);
        // #5083: the acked batch is the positive signal `loom-daemon status`
        // reports as `Observability: OK — last export …`.
        let snapshot = status.snapshot();
        assert!(snapshot.last_success_at.is_some());
        assert_eq!(snapshot.records_exported, 2);
        assert_eq!(snapshot.state, crate::types::ObservabilityExportState::Healthy);
    }

    #[tokio::test]
    async fn try_flush_leaves_the_queue_intact_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 10);
        queue.push(envelope());
        let exporter = FakeExporter::new(false);
        let status = status_cell();
        let outcome = try_flush(&queue, &exporter, 10, &status).await;
        assert_eq!(outcome, FlushOutcome::Failed);
        assert_eq!(queue.len(), 1, "a failed export must not lose the batch");
        // #5083: a first-ever flush that errors is `failing`, not `starting` —
        // there is a real error to report, not merely an absence of evidence.
        let snapshot = status.snapshot();
        assert_eq!(snapshot.consecutive_failures, 1);
        assert!(snapshot.last_success_at.is_none());
        assert!(snapshot
            .last_failure_detail
            .as_deref()
            .unwrap()
            .contains("sink down"));
        assert_eq!(snapshot.state, crate::types::ObservabilityExportState::Failing);
    }

    #[tokio::test]
    async fn kill_and_revive_drains_once_the_sink_is_reachable_again() {
        // The AC's "verified by a test that kills and revives the mock sink"
        // requirement, at the sender's own granularity.
        let dir = tempfile::tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 10);
        queue.push(envelope());
        queue.push(envelope());
        queue.push(envelope());
        let exporter = FakeExporter::new(false); // sink starts "killed"
        let status = status_cell();

        assert_eq!(try_flush(&queue, &exporter, 10, &status).await, FlushOutcome::Failed);
        assert_eq!(queue.len(), 3, "still queued while the sink is down");
        assert_eq!(status.snapshot().consecutive_failures, 1);

        exporter.set_up(true); // "revive" the sink
        assert_eq!(try_flush(&queue, &exporter, 10, &status).await, FlushOutcome::Sent(3));
        assert!(queue.is_empty());
        assert_eq!(exporter.received.lock().unwrap().len(), 3);
        // #5083: recovery clears the failure run, so the state goes back to
        // `healthy` rather than latching on the first bad night.
        let snapshot = status.snapshot();
        assert_eq!(snapshot.consecutive_failures, 0);
        assert_eq!(snapshot.records_exported, 3);
        assert_eq!(snapshot.state, crate::types::ObservabilityExportState::Healthy);
        // The failure detail is deliberately NOT cleared — "worked, broke,
        // recovered" stays legible after the fact.
        assert!(snapshot.last_failure_at.is_some());
    }

    #[tokio::test]
    async fn respects_batch_size_across_repeated_flushes() {
        let dir = tempfile::tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 10);
        for _ in 0..5 {
            queue.push(envelope());
        }
        let exporter = FakeExporter::new(true);
        let status = status_cell();
        assert_eq!(try_flush(&queue, &exporter, 2, &status).await, FlushOutcome::Sent(2));
        assert_eq!(queue.len(), 3);
        assert_eq!(try_flush(&queue, &exporter, 2, &status).await, FlushOutcome::Sent(2));
        assert_eq!(queue.len(), 1);
        assert_eq!(try_flush(&queue, &exporter, 2, &status).await, FlushOutcome::Sent(1));
        assert!(queue.is_empty());
        // #5083: the record count accumulates across flushes, so the status
        // line's "N record(s)" is a running total, not a per-batch figure.
        assert_eq!(status.snapshot().records_exported, 5);
    }

    // ------------------------------------------------------------------
    // jitter
    // ------------------------------------------------------------------

    #[test]
    fn jitter_stays_within_twenty_percent_of_base() {
        let mut rng = Rng::seeded(11);
        let base = Duration::from_secs(30);
        for _ in 0..50 {
            let jittered_value = jittered(base, &mut rng);
            let low = Duration::from_millis((base.as_millis() as u64) * 4 / 5);
            let high = Duration::from_millis((base.as_millis() as u64) * 6 / 5);
            assert!(
                jittered_value >= low && jittered_value <= high,
                "{jittered_value:?} out of [{low:?}, {high:?}] for base {base:?}"
            );
        }
    }

    #[test]
    fn jitter_of_zero_base_is_zero() {
        let mut rng = Rng::seeded(1);
        assert_eq!(jittered(Duration::ZERO, &mut rng), Duration::ZERO);
    }

    #[test]
    fn jitter_is_deterministic_for_a_seeded_rng() {
        let mut a = Rng::seeded(99);
        let mut b = Rng::seeded(99);
        let base = Duration::from_secs(10);
        assert_eq!(jittered(base, &mut a), jittered(base, &mut b));
    }

    /// The #5083 headline case, at the sender's own granularity: an exporter
    /// that has been up for hours with a queue that is *never* empty and a sink
    /// that never acks. Before #5083 this left no trace on any surface — the
    /// health section renders only on an id mismatch, and the on-disk queue
    /// looks the same whether it drained or never sent.
    #[tokio::test]
    async fn a_never_acking_sink_is_visible_as_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 10);
        queue.push(envelope());
        let exporter = FakeExporter::new(false);
        let status = status_cell();
        for _ in 0..5 {
            assert_eq!(try_flush(&queue, &exporter, 10, &status).await, FlushOutcome::Failed);
        }
        let snapshot = status.snapshot();
        assert_eq!(snapshot.consecutive_failures, 5);
        assert_eq!(snapshot.records_exported, 0);
        assert!(snapshot.state.is_problem(), "{:?}", snapshot.state);
    }
}
