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
//! silent loss and never unbounded growth (per the issue's AC). Every loss —
//! capacity eviction, a reopen front-trim, an exporter-reported rejection —
//! lands in this queue's durable [`super::loss_ledger`] by reason (Issue
//! #11527), so `dropped_total` survives a restart instead of resetting.
//!
//! **Peek-then-ack, not pop.** The sender ([`super::sender`]) reads a batch
//! via [`DurableQueue::peek_batch`] without removing it, attempts the export,
//! and only calls [`DurableQueue::ack`] on success — a failed export leaves
//! the batch queued for the next retry rather than losing it.
//!
//! **Append + compact journal (#11137).** An enqueue appends only the new
//! envelope's line (`O(1)` in queue length) and, on the durable paths, fsyncs
//! the file (the directory only when the file was just created). A
//! drop-oldest eviction changes in-memory state only; its line stays in the
//! file as a stale prefix. `ack`, `ack_snapshot` and a size-triggered
//! compaction (the file holding more than `2 x capacity` lines) rewrite the
//! file via temp + rename, so the amortised write cost per enqueue stays
//! `O(1)`. The first line is a `{"loom_queue_header":{"capacity":N}}` record
//! of the capacity the file was written under: on reopen the last `N` lines
//! are the live queue, and only a drop below that live set because the
//! configured capacity shrank counts as [`REASON_CAPACITY_SHRINK`] — stale
//! prefix lines were already counted as `queue_overflow` when evicted.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::loss_ledger::{LossLedger, REASON_CAPACITY_SHRINK, REASON_QUEUE_OVERFLOW};
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

/// Key of the first-line header recording the capacity a queue file was
/// written under (#11137). A line carrying it never parses as an envelope, so
/// older readers skip it as a corrupt line.
const HEADER_KEY: &str = "loom_queue_header";

struct QueueState {
    items: VecDeque<TelemetryEnvelope>,
    /// Envelope lines currently in the file, stale (evicted) prefix included.
    file_lines: usize,
    /// True once the file exists with a header, so appends need not write one.
    file_ready: bool,
    /// Bulk-backfill offers refused at the backfill limit (#11115).
    deferred_total: u64,
    head_sequence: u128,
}

/// Outcome of a bounded bulk-backfill offer (#11115).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Every sink took the record.
    Admitted,
    /// At least one sink was at its backfill limit; the caller must keep the
    /// record behind its cursor and retry on a later pass.
    Deferred,
}

/// Occupancy ceiling for bulk backfill: 75% of `capacity` (never above it).
/// The remaining quarter is headroom that ordinary lifecycle/CI producers
/// can use without evicting anything while a replay is in progress.
#[must_use]
pub fn backfill_limit(capacity: usize) -> usize {
    capacity - capacity / 4
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
    /// Durable, by-reason loss counts (Issue #11527): capacity evictions,
    /// reopen front-trims and exporter-reported losses all land here, and
    /// [`Self::dropped_total`] is its sum — so the count a `host.export`
    /// reports survives a restart.
    ledger: LossLedger,
    state: Mutex<QueueState>,
}

impl std::fmt::Debug for DurableQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableQueue")
            .field("path", &self.path)
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

/// The loss-ledger path for a queue at `queue_path`: the queue filename with
/// its extension replaced by `loss.json` — `observability-queue.jsonl` keeps
/// a ledger at `observability-queue.loss.json`, `observability-queue.otlp
/// .jsonl` at `observability-queue.otlp.loss.json`. One ledger per queue
/// file, derived (never configured), so every [`DurableQueue`] — including
/// the sole-exporter legacy name and the overhead journal — gets exactly one.
fn loss_ledger_path(queue_path: &Path) -> PathBuf {
    queue_path.with_extension("loss.json")
}

impl DurableQueue {
    /// Open (or create) the queue at `path`, replaying any envelopes left
    /// over from a prior process (best-effort — a missing, unreadable, or
    /// partially-corrupt file degrades to an empty queue rather than
    /// erroring; a corrupt individual line is skipped, matching
    /// [`crate::sweep_outcomes::read_all`]'s soft-fail contract). `capacity`
    /// is clamped to at least 1. Envelopes trimmed off the front by a
    /// capacity that shrank since the file was last written are recorded in
    /// the queue's loss ledger as [`REASON_CAPACITY_SHRINK`].
    #[must_use]
    pub fn open(path: PathBuf, capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let ledger = LossLedger::open(loss_ledger_path(&path));
        let loaded = load_existing(&path);
        let mut items = loaded.items;
        // A capacity that shrank since the file was last written (e.g. a
        // config edit between restarts) must not resurrect more than the
        // new bound allows. Stale prefix lines are not live records and were
        // already counted when evicted, so only the excess of the live set
        // over the new capacity is a shrink loss.
        let mut shrunk = 0u64;
        while items.len() > capacity {
            items.pop_front();
            shrunk += 1;
        }
        if shrunk > 0 {
            ledger.record(REASON_CAPACITY_SHRINK, shrunk);
        }
        // Rewrite when the file does not already describe exactly this queue
        // under this capacity (stale prefix, changed capacity, legacy file
        // without a header, a corrupt/torn line): otherwise every reopen would
        // re-trim the same front, and an append could glue onto a torn line.
        let mut file_lines = loaded.lines;
        let mut file_ready = loaded.header_capacity.is_some();
        let needs_rewrite = loaded.exists
            && (loaded.header_capacity != Some(capacity)
                || loaded.lines != items.len()
                || loaded.corrupt);
        if needs_rewrite {
            match persist_to(&path, &items, capacity, false) {
                Ok(()) => {
                    file_lines = items.len();
                    file_ready = true;
                }
                Err(error) => {
                    log::warn!("observability: failed to persist trimmed queue: {error}");
                }
            }
        }
        DurableQueue {
            path,
            capacity,
            ledger,
            state: Mutex::new(QueueState {
                items,
                file_lines,
                file_ready,
                deferred_total: 0,
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
        self.evict_if_full(&mut state);
        self.enqueue_locked(&mut state, envelope, durable)
    }

    /// Drop-oldest eviction for ordinary producers. Never reached by the
    /// bounded backfill path ([`Self::push_bounded`] / [`FanoutQueue`]).
    fn evict_if_full(&self, state: &mut QueueState) {
        if state.items.len() >= self.capacity {
            state.items.pop_front();
            state.head_sequence += 1;
            self.ledger.record(REASON_QUEUE_OVERFLOW, 1);
            log::warn!(
                "observability: queue at capacity ({}); dropped oldest record \
                 (queue_overflow, dropped_total={})",
                self.capacity,
                self.ledger.total()
            );
        }
    }

    /// Append `envelope` and persist, with the caller already holding this
    /// queue's lock (`state`). Never evicts: callers that need drop-oldest run
    /// [`Self::evict_if_full`] first under the same lock hold.
    fn enqueue_locked(
        &self,
        state: &mut QueueState,
        envelope: TelemetryEnvelope,
        durable: bool,
    ) -> std::io::Result<()> {
        state.items.push_back(envelope);
        let compact = state.file_lines >= 2 * self.capacity;
        if compact || !state.file_ready {
            // Compaction (or first write): one temp + rename that also
            // replaces the file, so the directory entry is synced too.
            persist_to(&self.path, &state.items, self.capacity, durable)?;
            state.file_lines = state.items.len();
            state.file_ready = true;
            return Ok(());
        }
        let line = state
            .items
            .back()
            .and_then(|item| serde_json::to_string(item).ok());
        if let Some(mut line) = line {
            line.push('\n');
            let appended = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .and_then(|mut file| {
                    write_counted(&mut file, line.as_bytes())?;
                    state.file_lines += 1;
                    if durable {
                        file.sync_all()?;
                    }
                    Ok(())
                });
            if appended.is_err() {
                // A failed append may leave a torn line or a missing record;
                // distrust the file so the next enqueue rewrites it from memory.
                state.file_ready = false;
            }
            return appended;
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

    /// Total envelopes (and, for `receiver_rejected`, signal items — see the
    /// reason vocabulary in [`super::loss_ledger`]) lost over this queue's
    /// lifetime, by every reason. **Durable** (Issue #11527): the sum of the
    /// on-disk loss ledger, so it survives daemon restarts instead of
    /// resetting to 0.
    #[must_use]
    pub fn dropped_total(&self) -> u64 {
        self.ledger.total()
    }

    /// The durable by-reason breakdown behind [`Self::dropped_total`], for
    /// `host.export`'s `dropped_by_reason` (Issue #11527).
    #[must_use]
    pub fn dropped_by_reason(&self) -> std::collections::BTreeMap<String, u64> {
        self.ledger.by_reason()
    }

    /// Record exporter-reported losses (Issue #11527): the sender feeds the
    /// [`super::outcome::BatchOutcome::lost`] map of every flush here, so a
    /// permanently rejected batch (e.g. OTLP 400) is counted durably by
    /// reason instead of vanishing between restarts.
    pub fn record_losses(&self, losses: &std::collections::BTreeMap<String, u64>) {
        self.ledger.record_losses(losses);
    }

    /// Total bulk-backfill offers refused at the backfill limit over this
    /// queue's lifetime (session-scoped). Deferral is not loss.
    #[must_use]
    pub fn deferred_total(&self) -> u64 {
        self.lock().deferred_total
    }

    /// Live pressure snapshot for the status path (#11115).
    #[must_use]
    pub fn pressure(&self) -> crate::types::ObservabilityQueuePressure {
        let state = self.lock();
        crate::types::ObservabilityQueuePressure {
            depth: state.items.len() as u64,
            capacity: self.capacity as u64,
            backfill_limit: backfill_limit(self.capacity) as u64,
            dropped_total: self.ledger.total(),
            backfill_deferred_total: state.deferred_total,
        }
    }

    /// True when a bulk-backfill offer fits below [`backfill_limit`]; counts a
    /// deferral when it does not. Takes the caller's lock guard so the answer
    /// stays true until the caller's enqueue under that same hold.
    fn backfill_room_locked(&self, state: &mut QueueState) -> bool {
        if state.items.len() < backfill_limit(self.capacity) {
            true
        } else {
            state.deferred_total += 1;
            false
        }
    }

    /// Bounded, non-evicting durable enqueue for bulk backfill (#11115):
    /// admits only while occupancy is below [`backfill_limit`]; otherwise
    /// counts a deferral and returns [`Admission::Deferred`] without touching
    /// the queue. Ordinary [`Self::push`] keeps its drop-oldest behavior.
    ///
    /// The limit check, the enqueue and the durable write all happen under a
    /// single hold of the queue lock, and this path never calls
    /// [`Self::evict_if_full`]: no concurrent producer can fill the queue
    /// between the check and the push, so an [`Admission::Admitted`] answer
    /// never coincides with an eviction.
    pub fn push_bounded(&self, envelope: TelemetryEnvelope) -> std::io::Result<Admission> {
        let mut state = self.lock();
        if !self.backfill_room_locked(&mut state) {
            return Ok(Admission::Deferred);
        }
        #[cfg(test)]
        tests::run_bounded_admit_probe(self);
        self.enqueue_locked(&mut state, envelope, true)?;
        Ok(Admission::Admitted)
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
        self.persist(&mut state);
    }

    /// Remove the front `n` envelopes (clamped to the current length) after a
    /// successful export, persisting the drained state to disk.
    pub fn ack(&self, n: usize) {
        let mut state = self.lock();
        let n = n.min(state.items.len());
        state.items.drain(0..n);
        state.head_sequence += n as u128;
        self.persist(&mut state);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Compact the file to exactly the in-memory queue (ack paths).
    fn persist(&self, state: &mut QueueState) {
        match persist_to(&self.path, &state.items, self.capacity, false) {
            Ok(()) => {
                state.file_lines = state.items.len();
                state.file_ready = true;
            }
            Err(error) => {
                // The file may still hold acked lines; force a rewrite next enqueue.
                state.file_ready = false;
                log::warn!(
                    "observability: failed to persist queue to {}: {error}",
                    self.path.display()
                );
            }
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

    /// Bounded admission for bulk journal replay (#11115). Unlike
    /// [`Self::offer_durable`] this must never evict already-queued records:
    /// a sink at its backfill limit answers [`Admission::Deferred`] and the
    /// caller leaves the record behind its persisted cursor. The default
    /// (for sinks with no capacity notion) is an unconditional durable offer.
    fn offer_backfill(&self, envelope: TelemetryEnvelope) -> std::io::Result<Admission> {
        self.offer_durable(envelope).map(|()| Admission::Admitted)
    }
}

impl QueueSink for DurableQueue {
    fn offer(&self, envelope: TelemetryEnvelope) {
        self.push(envelope);
    }

    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        self.push_durable(envelope)
    }

    fn offer_backfill(&self, envelope: TelemetryEnvelope) -> std::io::Result<Admission> {
        self.push_bounded(envelope)
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

    /// All-or-defer across unequal queues (#11115). Every queue's lock is
    /// taken first, in config order, and held through the checks and the
    /// pushes, so no producer can fill a queue between its check and its push
    /// and the push never evicts. Every queue is checked (no short-circuit:
    /// each saturated exporter counts its own deferral, so pressure is
    /// attributed per exporter), and the record is pushed only when **all**
    /// have room.
    ///
    /// Lock order is safe: `FanoutQueue` is the only holder of more than one
    /// queue lock, it always locks in config order, and a queue listed twice
    /// is locked (and offered) once.
    ///
    /// A persistence failure on a later queue still leaves earlier queues
    /// holding the record while the caller's cursor does not advance; the
    /// retry re-offers it, so delivery is at-least-once (a duplicate, never
    /// an omission).
    fn offer_backfill(&self, envelope: TelemetryEnvelope) -> std::io::Result<Admission> {
        let mut locked: Vec<(&DurableQueue, std::sync::MutexGuard<'_, QueueState>)> =
            Vec::with_capacity(self.queues.len());
        for queue in &self.queues {
            if locked.iter().any(|(held, _)| std::ptr::eq(*held, &**queue)) {
                continue;
            }
            locked.push((queue, queue.lock()));
        }
        let mut all_have_room = true;
        for (queue, state) in &mut locked {
            all_have_room &= queue.backfill_room_locked(state);
        }
        if !all_have_room {
            return Ok(Admission::Deferred);
        }
        for (queue, state) in &mut locked {
            #[cfg(test)]
            tests::run_bounded_admit_probe(queue);
            queue.enqueue_locked(state, envelope.clone(), true)?;
        }
        Ok(Admission::Admitted)
    }
}

/// What [`load_existing`] found in a queue file.
struct Loaded {
    /// The live queue: the last `header capacity` envelope lines (all of them
    /// for a legacy headerless file).
    items: VecDeque<TelemetryEnvelope>,
    /// Envelope lines in the file, stale prefix included.
    lines: usize,
    header_capacity: Option<usize>,
    /// A non-blank line that was neither an envelope nor the header.
    corrupt: bool,
    exists: bool,
}

fn load_existing(path: &Path) -> Loaded {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Loaded {
            items: VecDeque::new(),
            lines: 0,
            header_capacity: None,
            corrupt: false,
            exists: false,
        };
    };
    let mut items = VecDeque::new();
    let mut header_capacity = None;
    let mut corrupt = false;
    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        if let Ok(item) = serde_json::from_str::<TelemetryEnvelope>(line) {
            items.push_back(item);
        } else if let Some(capacity) = parse_header(line) {
            header_capacity = Some(capacity);
        } else {
            corrupt = true;
        }
    }
    let lines = items.len();
    if let Some(capacity) = header_capacity {
        while items.len() > capacity {
            items.pop_front();
        }
    }
    Loaded {
        items,
        lines,
        header_capacity,
        corrupt,
        exists: true,
    }
}

fn parse_header(line: &str) -> Option<usize> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let capacity = value.get(HEADER_KEY)?.get("capacity")?.as_u64()?;
    usize::try_from(capacity).ok()
}

/// Write `bytes`, counting them under test so the regression test can assert
/// the per-enqueue write cost without relying on wall time.
fn write_counted(file: &mut std::fs::File, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    tests::BYTES_WRITTEN.with(|total| total.set(total.get() + bytes.len()));
    file.write_all(bytes)
}

/// Atomically rewrite `path` to a header plus exactly `items`, one JSON object
/// per line — a temp-file-then-rename, mirroring
/// [`crate::idle_exit::write_marker`]. When `durable`, the temp file is synced
/// before the rename and the directory after it.
fn persist_to(
    path: &Path,
    items: &VecDeque<TelemetryEnvelope>,
    capacity: usize,
    durable: bool,
) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("jsonl.tmp-{}", std::process::id()));
    let mut buffer = format!("{{\"{HEADER_KEY}\":{{\"capacity\":{capacity}}}}}\n");
    for item in items {
        if let Ok(line) = serde_json::to_string(item) {
            buffer.push_str(&line);
            buffer.push('\n');
        }
    }
    let mut file = std::fs::File::create(&temporary)?;
    write_counted(&mut file, buffer.as_bytes())?;
    if durable {
        file.sync_all()?;
    }
    drop(file);
    std::fs::rename(temporary, path)?;
    if durable {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::telemetry::{RepoVisibility, SweepStartedRecord, TelemetryRecord};
    use serial_test::serial;
    use std::cell::Cell;
    use std::sync::TryLockError;
    use tempfile::tempdir;

    thread_local! {
        /// Test hook fired on the bounded path after the limit check and
        /// before the enqueue (#11115) — i.e. exactly where a racing producer
        /// used to slip in. Thread-local so parallel tests never see it.
        static BOUNDED_ADMIT_PROBE: Cell<Option<fn(&DurableQueue)>> = const { Cell::new(None) };
        static BOUNDED_ADMIT_PROBE_CALLS: Cell<usize> = const { Cell::new(0) };
        /// Bytes written to queue files by this thread (#11137 regression seam).
        pub(super) static BYTES_WRITTEN: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn run_bounded_admit_probe(queue: &DurableQueue) {
        if let Some(probe) = BOUNDED_ADMIT_PROBE.with(Cell::get) {
            BOUNDED_ADMIT_PROBE_CALLS.with(|calls| calls.set(calls.get() + 1));
            probe(queue);
        }
    }

    /// Arms a probe asserting that, between the limit check and the enqueue,
    /// the queue lock is still held: any other producer's `push` would block
    /// rather than fill the queue in the gap.
    fn arm_lock_held_probe() {
        fn assert_lock_held(queue: &DurableQueue) {
            assert!(
                matches!(queue.state.try_lock(), Err(TryLockError::WouldBlock)),
                "bounded admission released the queue lock between check and enqueue"
            );
        }
        BOUNDED_ADMIT_PROBE.with(|probe| probe.set(Some(assert_lock_held)));
        BOUNDED_ADMIT_PROBE_CALLS.with(|calls| calls.set(0));
    }

    fn disarm_probe() -> usize {
        BOUNDED_ADMIT_PROBE.with(|probe| probe.set(None));
        BOUNDED_ADMIT_PROBE_CALLS.with(Cell::get)
    }

    fn issues(queue: &DurableQueue) -> Vec<u32> {
        queue
            .peek_batch(usize::MAX)
            .iter()
            .map(|e| match &e.record {
                TelemetryRecord::SweepStarted(r) => r.issue,
                other => panic!("unexpected record {other:?}"),
            })
            .collect()
    }

    #[test]
    fn bounded_check_and_enqueue_share_one_lock_hold() {
        let dir = tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 8);
        for i in 1..=5 {
            queue.push(envelope(i));
        }
        arm_lock_held_probe();
        let admission = queue.push_bounded(envelope(100)).unwrap();
        assert_eq!(disarm_probe(), 1, "probe must fire between check and enqueue");
        assert_eq!(admission, Admission::Admitted);
        assert_eq!(queue.dropped_total(), 0);
    }

    #[test]
    fn reviewer_interleaving_never_evicts_on_backfill() {
        // Capacity 8, limit 6, depth 5: the judge's #11115 interleaving.
        let dir = tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 8);
        for i in 1..=5 {
            queue.push(envelope(i));
        }
        assert_eq!(queue.push_bounded(envelope(100)).unwrap(), Admission::Admitted);
        // Ordinary producers fill the headroom to capacity without evicting.
        queue.push(envelope(6));
        queue.push(envelope(7));
        assert_eq!(queue.len(), 8);
        assert_eq!(queue.dropped_total(), 0);
        // Backfill at hard capacity defers; it must not take the eviction branch.
        assert_eq!(queue.push_bounded(envelope(101)).unwrap(), Admission::Deferred);
        assert_eq!(queue.dropped_total(), 0, "bounded push evicted at capacity");
        assert_eq!(issues(&queue), vec![1, 2, 3, 4, 5, 100, 6, 7]);
        // Ordinary push still drops oldest.
        queue.push(envelope(8));
        assert_eq!(queue.dropped_total(), 1);
        assert_eq!(issues(&queue)[0], 2);
    }

    #[test]
    fn bounded_push_never_evicts_at_any_depth() {
        let dir = tempdir().unwrap();
        for depth in 0..=8u32 {
            let queue = DurableQueue::open(dir.path().join(format!("q{depth}.jsonl")), 8);
            for i in 0..depth {
                queue.push(envelope(i));
            }
            let before = issues(&queue);
            let admission = queue.push_bounded(envelope(999)).unwrap();
            assert_eq!(queue.dropped_total(), 0, "depth {depth}");
            assert_eq!(&issues(&queue)[..before.len()], &before[..], "depth {depth}");
            let expect = if depth < 6 {
                Admission::Admitted
            } else {
                Admission::Deferred
            };
            assert_eq!(admission, expect, "depth {depth}");
        }
    }

    #[test]
    fn concurrent_bounded_callers_cannot_overshoot_the_limit() {
        let dir = tempdir().unwrap();
        let queue = Arc::new(DurableQueue::open(dir.path().join("q.jsonl"), 8));
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16u32)
            .map(|i| {
                let queue = Arc::clone(&queue);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    queue.push_bounded(envelope(i)).unwrap()
                })
            })
            .collect();
        let admitted = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|a| *a == Admission::Admitted)
            .count();
        assert_eq!(admitted, 6, "exactly backfill_limit(8) offers admitted");
        assert_eq!(queue.len(), 6);
        assert_eq!(queue.dropped_total(), 0);
        assert_eq!(queue.deferred_total(), 10);
    }

    #[test]
    fn fanout_backfill_holds_every_queue_lock_through_enqueue() {
        let dir = tempdir().unwrap();
        let a = Arc::new(DurableQueue::open(dir.path().join("q.a.jsonl"), 8));
        let b = Arc::new(DurableQueue::open(dir.path().join("q.b.jsonl"), 4));
        let fanout = FanoutQueue::new(vec![a.clone(), b.clone()]);
        arm_lock_held_probe();
        assert_eq!(fanout.offer_backfill(envelope(1)).unwrap(), Admission::Admitted);
        assert_eq!(disarm_probe(), 2, "probe fires once per queue");
        // Smaller queue reaches its limit (3) first: all-or-defer, no eviction.
        fanout.offer_backfill(envelope(2)).unwrap();
        fanout.offer_backfill(envelope(3)).unwrap();
        assert_eq!(fanout.offer_backfill(envelope(4)).unwrap(), Admission::Deferred);
        assert_eq!((a.len(), b.len()), (3, 3));
        assert_eq!((a.dropped_total(), b.dropped_total()), (0, 0));
        assert_eq!((a.deferred_total(), b.deferred_total()), (0, 1));
    }

    #[test]
    fn fanout_backfill_with_a_repeated_queue_does_not_deadlock() {
        let dir = tempdir().unwrap();
        let a = Arc::new(DurableQueue::open(dir.path().join("q.a.jsonl"), 8));
        let fanout = FanoutQueue::new(vec![a.clone(), a.clone()]);
        assert_eq!(fanout.offer_backfill(envelope(1)).unwrap(), Admission::Admitted);
        assert_eq!(a.len(), 1);
    }

    #[test]
    fn fanout_backfill_persist_failure_is_at_least_once() {
        let dir = tempdir().unwrap();
        let ok = Arc::new(DurableQueue::open(dir.path().join("q.https.jsonl"), 8));
        let bad_path = dir.path().join("not-a-file");
        std::fs::create_dir_all(&bad_path).unwrap();
        let bad = Arc::new(DurableQueue::open(bad_path, 8));
        let fanout = FanoutQueue::new(vec![ok.clone(), bad]);
        // Err: the caller keeps its cursor and retries; the earlier queue
        // already holds the record, so the retry yields a duplicate there,
        // never an omission.
        assert!(fanout.offer_backfill(envelope(1)).is_err());
        assert_eq!(ok.len(), 1);
        assert_eq!(ok.dropped_total(), 0);
    }

    fn envelope(issue: u32) -> TelemetryEnvelope {
        TelemetryEnvelope::new(
            "host-test",
            TelemetryRecord::SweepStarted(SweepStartedRecord {
                story_points: None,
                repo: "rjwalters/loom".to_string(),
                visibility: RepoVisibility::Public,
                issue,
                sweep_id: format!("sweep-issue-{issue}-0"),
                started_at: chrono::Utc::now(),
                facts: Default::default(),
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
        assert_eq!(
            queue.dropped_by_reason(),
            std::collections::BTreeMap::from([("queue_overflow".to_string(), 1)])
        );
        let batch = queue.peek_batch(10);
        match &batch[0].record {
            TelemetryRecord::SweepStarted(r) => assert_eq!(r.issue, 2),
            other => panic!("unexpected record {other:?}"),
        }
        queue.push(envelope(4));
        queue.push(envelope(5));
        assert_eq!(queue.dropped_total(), 3, "never grows unbounded — every overflow is counted");
        assert_eq!(queue.len(), 2);
        // The count is durable (Issue #11527): a reopen over the same files
        // sees the same ledger, it does not restart at 0.
        let reopened = DurableQueue::open(dir.path().join("q.jsonl"), 2);
        assert_eq!(reopened.dropped_total(), 3);
        assert_eq!(reopened.dropped_by_reason().get("queue_overflow"), Some(&3));
    }

    #[test]
    fn exporter_reported_losses_land_in_the_ledger_by_reason() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        let queue = DurableQueue::open(path.clone(), 10);
        queue.record_losses(&std::collections::BTreeMap::from([
            ("status_400".to_string(), 2),
            ("receiver_rejected".to_string(), 5),
        ]));
        assert_eq!(queue.dropped_total(), 7, "total = the sum over reasons");
        assert_eq!(queue.dropped_by_reason().get("status_400"), Some(&2));
        let reopened = DurableQueue::open(path, 10);
        assert_eq!(reopened.dropped_by_reason().get("status_400"), Some(&2));
        assert_eq!(reopened.dropped_by_reason().get("receiver_rejected"), Some(&5));
        assert_eq!(reopened.dropped_total(), 7);
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
        // The front-trim is a counted, durable loss (Issue #11527).
        assert_eq!(reopened.dropped_total(), 3);
        assert_eq!(reopened.dropped_by_reason().get("capacity_shrink"), Some(&3));
        let reopened_again = DurableQueue::open(dir.path().join("q.jsonl"), 2);
        assert_eq!(
            reopened_again.dropped_by_reason().get("capacity_shrink"),
            Some(&3),
            "no double count on a second reopen at the same capacity"
        );
    }

    #[test]
    fn durable_enqueue_write_cost_is_o_of_k_not_k_times_capacity() {
        let dir = tempdir().unwrap();
        let capacity = 200;
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), capacity);
        let line_len = serde_json::to_string(&envelope(1)).unwrap().len() + 1;
        let records = 2_000; // K >> C
        BYTES_WRITTEN.with(|total| total.set(0));
        for i in 0..records {
            queue.push_durable(envelope(i as u32)).unwrap();
        }
        let written = BYTES_WRITTEN.with(Cell::get);
        assert!(
            written < 3 * records * line_len,
            "wrote {written} bytes for {records} offers of {line_len}-byte lines \
             (capacity {capacity}); expected O(K), not O(K x C)"
        );
        assert_eq!(queue.len(), capacity);
    }

    #[test]
    fn reopen_after_evictions_does_not_recount_stale_prefix() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        {
            let queue = DurableQueue::open(path.clone(), 4);
            for i in 1..=7 {
                queue.push(envelope(i));
            }
            assert_eq!(queue.dropped_total(), 3);
        }
        let reopened = DurableQueue::open(path.clone(), 4);
        assert_eq!(reopened.dropped_total(), 3, "stale prefix must not count as capacity_shrink");
        assert_eq!(issues(&reopened), vec![4, 5, 6, 7]);
        let again = DurableQueue::open(path, 4);
        assert_eq!(again.dropped_total(), 3);
        assert_eq!(issues(&again), vec![4, 5, 6, 7]);
    }

    #[test]
    fn acked_records_are_not_resurrected_after_crash_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        {
            let queue = DurableQueue::open(path.clone(), 10);
            for i in 1..=5 {
                queue.push(envelope(i));
            }
            queue.ack(2);
            queue.push(envelope(6));
            // Dropped without a clean shutdown.
        }
        let reopened = DurableQueue::open(path, 10);
        assert_eq!(issues(&reopened), vec![3, 4, 5, 6]);
        assert_eq!(reopened.dropped_total(), 0);
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

    #[test]
    fn torn_append_is_healed_by_the_next_enqueue() {
        use std::io::Write;
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        let queue = DurableQueue::open(path.clone(), 8);
        queue.push(envelope(1));
        queue.push(envelope(2));
        // Simulate a failed append that left a partial line with no newline,
        // and the bookkeeping the failure path performs.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"partial").unwrap();
        queue.lock().file_ready = false;
        queue.push_durable(envelope(3)).unwrap();
        drop(queue);
        let reopened = DurableQueue::open(path, 8);
        assert_eq!(issues(&reopened), vec![1, 2, 3]);
    }

    #[test]
    fn failed_append_distrusts_the_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        let queue = DurableQueue::open(path.clone(), 8);
        queue.push(envelope(1));
        assert!(queue.lock().file_ready);
        // Replace the file with a directory so the append open fails.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(queue.push_durable(envelope(2)).is_err());
        assert!(!queue.lock().file_ready, "a failed append must force a rewrite");
        std::fs::remove_dir(&path).unwrap();
        queue.push_durable(envelope(3)).unwrap();
        drop(queue);
        let reopened = DurableQueue::open(path, 8);
        assert_eq!(issues(&reopened), vec![1, 2, 3]);
    }
}
