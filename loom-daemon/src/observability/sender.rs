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
use crate::telemetry::TelemetryEnvelope;

/// Starting backoff after a failed export attempt.
pub const MIN_BACKOFF: Duration = Duration::from_secs(5);
/// Backoff ceiling — an unreachable sink never waits longer than this
/// between retries.
pub const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// Consecutive failed flushes after which the per-flush diagnostic warns stop
/// being enough and the escalated operator warning fires (issue #9950). At
/// the [`MAX_BACKOFF`] ceiling, 12 consecutive failures is ~1h of continuous
/// dead sink — an hour of silently accumulating telemetry is where "transport
/// noise" becomes "the operator needs to go fix this".
pub const ESCALATE_AFTER_FAILURES: u32 = 12;
/// Re-fire cadence after the first escalation, in further consecutive
/// failures. At the backoff ceiling this is ~2h: the reminder exists so a
/// multi-hour outage (the 2026-10-01 store-tunnel shape — every record the
/// daemon produced trapped locally while nothing said so) keeps a heartbeat
/// in daemon.log without re-firing per flush.
pub const ESCALATE_EVERY_FAILURES: u32 = 24;

/// Most bytes of envelopes (as the JSON array the HTTPS exporter POSTs) one
/// batch carries (#10928), under `/ingest`'s 5 MiB body limit. `batch_size`
/// counts envelopes, and a few large queued records (the since-removed
/// `eta.snapshot` was capped at 1 MiB each, one per 5-minute pass
/// while the sink was down) would otherwise make a batch the sink answers
/// 413 to on every retry, wedging the queue for good.
pub const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// Bytes of `value`'s compact JSON, counted without buffering it.
pub(crate) fn json_len<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    // Writing to a counter cannot fail, and a value that fails to serialize
    // is rejected by the exporter itself; its partial length is harmless.
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}

/// How many envelopes from the front of `batch` fit in `budget` bytes of
/// JSON array. Always at least one: a lone envelope over the budget is still
/// offered, and the sink decides. Stops serializing at the first that does
/// not fit.
fn byte_bounded_len(batch: &[TelemetryEnvelope], budget: usize) -> usize {
    // `[` and `]`, then each envelope plus its comma.
    let mut used = 2;
    for (i, envelope) in batch.iter().enumerate() {
        used += json_len(envelope) + usize::from(i > 0);
        if i > 0 && used > budget {
            return i;
        }
    }
    batch.len()
}

/// Whether the `consecutive_failures`-th failed flush should fire the
/// escalated warning. Pure so the cadence is unit-testable: first at
/// [`ESCALATE_AFTER_FAILURES`], then every [`ESCALATE_EVERY_FAILURES`] after.
fn should_escalate(consecutive_failures: u32) -> bool {
    consecutive_failures >= ESCALATE_AFTER_FAILURES
        && (consecutive_failures - ESCALATE_AFTER_FAILURES).is_multiple_of(ESCALATE_EVERY_FAILURES)
}

/// Outcome of one [`try_flush`] attempt, for the caller's retry/backoff
/// decision.
#[derive(Debug, PartialEq, Eq)]
pub enum FlushOutcome {
    /// The queue was empty — nothing to send.
    Empty,
    /// This many envelopes were resolved (exported or permanently dropped).
    Sent(usize),
    /// The export attempt failed; the batch remains queued for retry.
    Failed,
}

/// Attempt to send one batch (up to `batch_size` envelopes and
/// [`MAX_BATCH_BYTES`], peeked from the front of `queue`) via `exporter`. Only acks (removes) the batch from
/// `queue` for the prefix acknowledged by the exporter. A retryable failure
/// preserves the suffix; permanent OTLP rejection/drop advances the prefix.
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
    let mut snapshot = queue.peek_snapshot(batch_size);
    // Only the prefix is sent, so only the prefix can be acked.
    let fit = byte_bounded_len(&snapshot.envelopes, MAX_BATCH_BYTES);
    snapshot.envelopes.truncate(fit);
    let batch = &snapshot.envelopes;
    if batch.is_empty() {
        return FlushOutcome::Empty;
    }
    let outcome = exporter.emit_batch_outcome(batch).await;
    let acknowledged = outcome.acknowledged.min(batch.len());
    queue.ack_snapshot(&snapshot, acknowledged);
    status.record_signals(&outcome.signals);
    if outcome.exported > 0 {
        status.record_success(outcome.exported);
    }
    if let Some(error) = outcome.error {
        log::warn!(
            "observability: export diagnostic, {} envelope(s) remain queued: {error}",
            queue.len()
        );
        status.record_failure(&error.to_string());
        // The per-flush diagnostic above is transport noise — the NUMBER of
        // near-identical lines carries the only signal, and nobody reads
        // that. #9950: at sustained-failure thresholds, say plainly that
        // telemetry is not landing, for how long, and WHAT TO DO — the
        // 2026-10-01 store-tunnel outage produced ~15h of diagnostic lines
        // and not one of them told the operator to go fix it.
        let snap = status.snapshot();
        if should_escalate(snap.consecutive_failures) {
            let since_success = snap.last_success_age_secs(chrono::Utc::now()).map_or_else(
                || "never had a successful export".to_string(),
                |age| format!("last success {} ago", crate::health::format_window(age)),
            );
            log::warn!(
                "observability: telemetry is NOT reaching {} — {} consecutive failed flush(es), \
                 {since_success}, {} envelope(s) accumulating locally. ACTION REQUIRED: check \
                 this host's OTel egress (edge collector / tunnel / ingest key) and the endpoint \
                 itself; `loom-daemon health` has the verdict, `loom-daemon status` the live \
                 line. (This escalation repeats roughly every 2h of continuous failure.)",
                snap.endpoint
                    .as_deref()
                    .unwrap_or("the configured endpoint"),
                snap.consecutive_failures,
                queue.len(),
            );
        }
    }
    if acknowledged == batch.len() {
        // Includes non-retryable drops: advance so a poison request cannot starve the queue.
        FlushOutcome::Sent(acknowledged)
    } else {
        FlushOutcome::Failed
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
    super::shutdown::spawn_sender(queue, exporter, batch_size, flush_interval, status)
}

pub(super) async fn run_sender<E: Exporter>(
    queue: Arc<DurableQueue>,
    exporter: &E,
    batch_size: usize,
    flush_interval: Duration,
    status: Arc<ExportStatus>,
    mut shutdown: tokio::sync::mpsc::Receiver<super::shutdown::Request>,
) {
    let mut rng = Rng::from_entropy();
    let mut backoff = MIN_BACKOFF;
    loop {
        if !super::shutdown::pause(
            &queue,
            exporter,
            batch_size,
            &status,
            &mut shutdown,
            jittered(flush_interval, &mut rng),
        )
        .await
        {
            return;
        }
        // Drain every currently-queued batch before sleeping again, so a
        // burst that arrived between ticks does not wait a full extra
        // `flush_interval` per batch.
        loop {
            let Some(outcome) =
                super::shutdown::flush(&queue, exporter, batch_size, &status, &mut shutdown).await
            else {
                return;
            };
            match outcome {
                FlushOutcome::Empty => {
                    backoff = MIN_BACKOFF;
                    break;
                }
                FlushOutcome::Sent(_) => {
                    backoff = MIN_BACKOFF;
                    // Loop again immediately — more may still be queued.
                }
                FlushOutcome::Failed => {
                    if !super::shutdown::pause(
                        &queue,
                        exporter,
                        batch_size,
                        &status,
                        &mut shutdown,
                        jittered(backoff, &mut rng),
                    )
                    .await
                    {
                        return;
                    }
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
                protection: None,
                admission_brake: None,
                is_captain: None,
                armed_singleton_jobs: Vec::new(),
                captainless_singleton_jobs: Vec::new(),
                exported_kinds: Vec::new(),
                exporters: Vec::new(),
                memory: None,
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
    async fn an_acked_batch_into_a_local_edge_collector_is_healthy_but_first_hop_only() {
        // #9015, the incident shape reproduced in-process: the exporter POSTs
        // to a LOCAL otel-edge collector that accepts everything. `healthy` is
        // the correct verdict for the hop the daemon can see — and the record
        // must simultaneously say that is ALL it saw, because on 2026-09-24
        // that edge's own egress was dead for 30h+ and nothing reached SigNoz.
        let dir = tempfile::tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 10);
        queue.push(envelope());
        let status =
            ExportStatus::started("host-test", "http://127.0.0.1:14318/v1/logs", "otlp", 30);
        assert_eq!(
            try_flush(&queue, &FakeExporter::new(true), 10, &status).await,
            FlushOutcome::Sent(1)
        );
        let snapshot = status.snapshot();
        assert_eq!(snapshot.state, crate::types::ObservabilityExportState::Healthy);
        assert_eq!(snapshot.scope, crate::types::ObservabilityExportScope::FirstHop);
        assert!(
            snapshot.endpoint_loopback,
            "an ack from a local collector must be published as a local hop"
        );
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

    /// #10928: a batch is cut by bytes as well as count, so queued large
    /// envelopes go out over several POSTs instead of one the sink rejects
    /// forever; a lone envelope over the budget still goes out.
    #[tokio::test]
    async fn a_batch_is_cut_at_the_byte_budget_and_never_below_one_envelope() {
        let one = json_len(&envelope());
        let batch = vec![envelope(), envelope(), envelope()];
        assert_eq!(byte_bounded_len(&batch, 2 + 3 * one + 2), 3);
        assert_eq!(byte_bounded_len(&batch, 2 + 2 * one + 1), 2);
        assert_eq!(byte_bounded_len(&batch, 2 + 2 * one), 1);
        assert_eq!(byte_bounded_len(&batch, 1), 1, "never zero: the sink decides");
        assert_eq!(byte_bounded_len(&[], 1), 0);
        assert_eq!(serde_json::to_vec(&batch).unwrap().len(), 2 + 3 * one + 2);

        // Through try_flush with the real budget: envelopes just over a
        // third of it go out one or two per POST, all acked in order.
        let dir = tempfile::tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 10);
        let mut big = envelope();
        if let TelemetryRecord::HostHealth(h) = &mut big.record {
            h.halt_reason = Some("x".repeat(MAX_BATCH_BYTES / 3 + 1));
        }
        for _ in 0..5 {
            queue.push(big.clone());
        }
        let exporter = FakeExporter::new(true);
        let status = status_cell();
        let mut posts = Vec::new();
        while let FlushOutcome::Sent(n) = try_flush(&queue, &exporter, 50, &status).await {
            posts.push(n);
        }
        assert_eq!(posts, vec![2, 2, 1]);
        assert!(queue.is_empty());
        assert_eq!(exporter.received.lock().unwrap().len(), 5);
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

    // --- sustained-failure escalation (#9950) ------------------------------

    #[test]
    fn escalation_cadence_fires_at_12_then_every_24() {
        for n in 0..12 {
            assert!(!should_escalate(n), "n={n} must stay diagnostic-only");
        }
        assert!(should_escalate(12), "the ~1h mark must escalate");
        for n in 13..36 {
            assert!(!should_escalate(n), "n={n} must not re-fire yet");
        }
        assert!(should_escalate(36), "~2h after the first escalation");
        assert!(!should_escalate(37));
        assert!(should_escalate(60), "the cadence keeps its ~2h heartbeat");
    }

    /// Drive `n` failed flushes on a dedicated thread with its own
    /// current-thread runtime, capturing what they logged. `capture_logs` is
    /// thread-local by design, and a runtime cannot be `block_on`-ed from
    /// inside another runtime — so the capture, the runtime and the flushes
    /// all live on this one spawned thread.
    fn flush_n_times_logging(n: usize) -> Vec<(log::Level, String)> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let dir = tempfile::tempdir().unwrap();
            let queue = DurableQueue::open(dir.path().join("q.jsonl"), 10);
            queue.push(envelope());
            let exporter = FakeExporter::new(false);
            let status = status_cell();
            let records = crate::test_log_capture::capture_logs(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    for _ in 0..n {
                        try_flush(&queue, &exporter, 1, &status).await;
                    }
                });
            });
            tx.send(records).unwrap();
        });
        rx.recv().unwrap()
    }

    fn escalation_lines(records: &[(log::Level, String)]) -> Vec<&(log::Level, String)> {
        records
            .iter()
            .filter(|(_, m)| m.contains("NOT reaching"))
            .collect()
    }

    #[test]
    fn diagnostic_warns_stay_below_the_escalation_threshold() {
        let records = flush_n_times_logging(11);
        assert_eq!(escalation_lines(&records).len(), 0, "11 failures must not escalate");
        // …but the per-flush diagnostic still fired every time.
        assert_eq!(
            records
                .iter()
                .filter(|(_, m)| m.contains("export diagnostic"))
                .count(),
            11
        );
    }

    #[test]
    fn the_escalated_warning_says_what_broke_and_what_to_do() {
        let records = flush_n_times_logging(12);
        let lines = escalation_lines(&records);
        assert_eq!(lines.len(), 1, "exactly one escalation at #12: {records:?}");
        let (level, msg) = lines[0];
        assert_eq!(*level, log::Level::Warn, "escalation must be a WARN");
        for want in [
            "NOT reaching https://example.invalid/ingest", // names the endpoint
            "12 consecutive failed flush(es)",             // names the streak
            "never had a successful export",               // the status_cell has none
            "1 envelope(s) accumulating",                  // names the backlog
            "ACTION REQUIRED",                             // says this is for the operator
            "edge collector / tunnel / ingest key",        // names the fix surfaces
            "loom-daemon health",                          // and where the verdict lives
        ] {
            assert!(msg.contains(want), "escalation missing {want:?}: {msg}");
        }
    }

    #[test]
    fn escalation_re_fires_on_the_cadence_not_per_flush() {
        let records = flush_n_times_logging(36);
        assert_eq!(escalation_lines(&records).len(), 2, "escalations at #12 and #36 only");
    }

    #[test]
    fn a_success_resets_the_escalation_clock() {
        let dir = tempfile::tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 10);
        queue.push(envelope());
        let exporter = FakeExporter::new(false);
        let status = status_cell();
        // 12 failures → one escalation, then recovery: the next streak starts
        // from zero, so 11 more failures must stay diagnostic-only.
        let records = crate::test_log_capture::capture_logs(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                for _ in 0..12 {
                    try_flush(&queue, &exporter, 1, &status).await;
                }
                exporter.set_up(true);
                assert_eq!(try_flush(&queue, &exporter, 1, &status).await, FlushOutcome::Sent(1));
                // Refill: a success consumed the queued envelope.
                queue.push(envelope());
                exporter.set_up(false);
                for _ in 0..11 {
                    try_flush(&queue, &exporter, 1, &status).await;
                }
            });
        });
        assert_eq!(escalation_lines(&records).len(), 1, "one escalation, then a clean streak");
    }
}
