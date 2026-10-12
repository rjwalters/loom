//! Durable, disk-backed offline queue for [`TelemetryEnvelope`] records
//! (Epic #4702, Phase 1 — issue #4705; spill tail — issue #11556).
//!
//! Fleet hosts sleep and idle-shutdown (#4467/#4697), so an in-memory-only
//! queue would silently lose every unsent record across a host's downtime
//! window. This queue instead keeps every record on disk and replays it in
//! [`DurableQueue::open`], so a record enqueued just before a clean daemon
//! exit (the idle-exit path never kills `-9`) is still there to drain on the
//! next boot.
//!
//! **Head window + append-only spill tail — never drop-oldest.** The queue is
//! two files, read in FIFO order:
//!
//! 1. The **head** (`<workspace>/.loom/logs/observability-queue.jsonl`,
//!    override via [`QUEUE_PATH_ENV`]): at most `capacity` envelopes, the hot
//!    window the sender peeks from. It is rewritten whole on every mutation
//!    with the create-temp + atomic-rename pattern
//!    [`crate::idle_exit::write_marker`] uses — `O(capacity)` per write.
//! 2. The **spill tail** (`<queue>.spill.jsonl`): once the head is full (or
//!    anything is already spilled, to keep FIFO), [`DurableQueue::push`]
//!    appends exactly one JSONL line with `O_APPEND` — `O(1)` bytes written
//!    per push, nothing rewritten. A persisted read offset
//!    (`<queue>.spill.offset`, written atomically) marks how much of the tail
//!    has been moved back into the head.
//!
//! [`DurableQueue::ack`] / [`DurableQueue::ack_snapshot`] refill the head from
//! the spill tail up to `capacity`. When the offset reaches the end of the
//! tail both spill files are removed; when it passes half the tail, the tail
//! is compacted (rewritten from the offset). A reopen with a shrunk
//! `capacity` moves the head's excess onto the front of the tail instead of
//! trimming it. [`DurableQueue::len`] counts head + tail, so
//! `host.export.queue_depth` and `status` see the whole backlog.
//!
//! **No byte cap** is configured (#10196 "no caps"). The **only** drop is a
//! spill write that fails (full or unwritable disk): it is recorded in this
//! queue's durable [`super::loss_ledger`] as [`REASON_SPILL_WRITE_FAILED`]
//! and logged at WARN with the count — never silent (Issue #11527).
//!
//! **Crash safety prefers duplicates over loss.** Every write is ordered so a
//! crash between two steps replays records twice rather than dropping them:
//! the head is persisted before the offset advances, a compaction resets the
//! offset to 0 before the compacted tail replaces the old one, and the
//! spilled count is re-derived from the tail file (minus the offset) on
//! reopen rather than trusted from memory. A torn final line (a crash mid
//! append) is cut off on reopen.
//!
//! **Peek-then-ack, not pop.** The sender ([`super::sender`]) reads a batch
//! via [`DurableQueue::peek_snapshot`] without removing it, attempts the
//! export, and only acks the confirmed prefix — a failed export leaves the
//! batch queued for the next retry rather than losing it. Batches are peeked
//! from the head only, and the sender bounds them by bytes
//! (`sender::MAX_BATCH_BYTES`, #10928), so a refill from spill never makes a
//! batch the sink cannot accept.

use std::collections::VecDeque;
use std::io::{BufRead, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::loss_ledger::LossLedger;
use crate::telemetry::TelemetryEnvelope;

/// Env var overriding the queue file path (test seam), mirroring
/// [`crate::sweep_outcomes::OUTCOMES_JOURNAL_PATH_ENV`].
pub const QUEUE_PATH_ENV: &str = "LOOM_OBSERVABILITY_QUEUE_PATH";

/// Default filename under `<workspace_root>/.loom/logs/`.
pub const QUEUE_FILENAME: &str = "observability-queue.jsonl";

/// Loss reason (Issue #11556): an envelope that had to spill past the head
/// window could not be appended to the spill tail (full or unwritable disk),
/// so it was dropped. The only way this queue loses a record. Envelope units.
pub const REASON_SPILL_WRITE_FAILED: &str = "spill_write_failed";

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

/// In-memory view of the spill tail: how many records remain past `offset`,
/// and the tail file's length as this process last wrote it.
#[derive(Debug, Default, Clone, Copy)]
struct SpillState {
    count: u64,
    offset: u64,
    bytes: u64,
}

struct QueueState {
    items: VecDeque<TelemetryEnvelope>,
    /// FIFO position of `items[0]`. Advanced only by acks — nothing is
    /// evicted from the front any more — so refills and pushes behind an
    /// in-flight snapshot get later positions and are never acked by it.
    head_sequence: u128,
    spill: SpillState,
}

/// An in-process queue cursor plus immutable payload snapshot. Sequence identity
/// distinguishes even byte-identical envelopes pushed while an export is in flight.
pub struct QueueSnapshot {
    start_sequence: u128,
    pub envelopes: Vec<TelemetryEnvelope>,
}

/// A disk-backed FIFO queue of [`TelemetryEnvelope`]s: a bounded head window
/// plus an append-only spill tail (see the module docs).
pub struct DurableQueue {
    path: PathBuf,
    spill_path: PathBuf,
    offset_path: PathBuf,
    capacity: usize,
    /// Durable, by-reason loss counts (Issue #11527): failed spill writes and
    /// exporter-reported losses all land here, and [`Self::dropped_total`]
    /// is its sum — so the count a `host.export` reports survives a restart.
    ledger: LossLedger,
    state: Mutex<QueueState>,
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

/// The append-only spill tail for a queue at `queue_path`
/// (`observability-queue.jsonl` → `observability-queue.spill.jsonl`).
fn spill_path(queue_path: &Path) -> PathBuf {
    queue_path.with_extension("spill.jsonl")
}

/// The spill tail's persisted read offset (`observability-queue.spill.offset`).
fn spill_offset_path(queue_path: &Path) -> PathBuf {
    queue_path.with_extension("spill.offset")
}

impl DurableQueue {
    /// Open (or create) the queue at `path`, replaying the head and spill tail
    /// left over from a prior process (best-effort — a missing, unreadable, or
    /// partially-corrupt file degrades to an empty queue rather than
    /// erroring; a corrupt individual line is skipped, matching
    /// [`crate::sweep_outcomes::read_all`]'s soft-fail contract). `capacity`
    /// is clamped to at least 1. A head larger than a capacity that shrank
    /// since the file was last written moves its excess onto the front of the
    /// spill tail — nothing is trimmed — and a head with room left (a grown
    /// capacity, or a crash mid-refill) is refilled from the tail.
    #[must_use]
    pub fn open(path: PathBuf, capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let ledger = LossLedger::open(loss_ledger_path(&path));
        let spill_path = spill_path(&path);
        let offset_path = spill_offset_path(&path);
        let mut items = load_existing(&path);
        let mut spill = open_spill(&spill_path, &offset_path);
        if items.len() > capacity {
            let excess = items.split_off(capacity);
            match prepend_to_spill(&spill_path, &offset_path, &excess, spill) {
                Ok(moved) => {
                    spill = moved;
                    // The excess is durably at the front of the tail; only
                    // now shrink the head file. A crash in between replays
                    // the excess twice, never zero times.
                    if let Err(error) = persist_to(&path, &items) {
                        log::warn!("observability: failed to persist shrunk queue head: {error}");
                    }
                }
                Err(error) => {
                    // Keep the oversized head in memory (and on disk) rather
                    // than lose it; pushes spill behind it and acks drain it.
                    log::warn!(
                        "observability: failed to move {} queued record(s) past the shrunk \
                         capacity to the spill tail: {error}",
                        excess.len()
                    );
                    items.extend(excess);
                }
            }
        }
        let queue = DurableQueue {
            path,
            spill_path,
            offset_path,
            capacity,
            ledger,
            state: Mutex::new(QueueState {
                items,
                head_sequence: 0,
                spill,
            }),
        };
        {
            let mut state = queue.lock();
            if state.items.len() < capacity && state.spill.count > 0 {
                queue.refill_and_persist(&mut state);
            }
        }
        queue
    }

    /// Enqueue `envelope`: into the head while it has room and nothing is
    /// spilled, else as one appended line on the spill tail. Persists
    /// best-effort — a head write failure is logged and swallowed (matches
    /// [`crate::sweep_outcomes::append_outcome`]'s "never block the caller on
    /// a journal-write failure" contract); a spill write failure is the one
    /// counted loss ([`REASON_SPILL_WRITE_FAILED`]).
    pub fn push(&self, envelope: TelemetryEnvelope) {
        if let Err(error) = self.push_inner(envelope, false) {
            log::warn!("observability: failed to persist queue: {error}");
        }
    }

    /// Offer a terminal span and confirm its queue file and directory reached
    /// stable storage before the caller removes active execution context.
    /// On failure the in-memory offer remains (head) or is counted lost
    /// (spill); either way, retain context for recovery.
    pub fn push_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        self.push_inner(envelope, true)
    }

    fn push_inner(&self, envelope: TelemetryEnvelope, durable: bool) -> std::io::Result<()> {
        let mut state = self.lock();
        if state.spill.count == 0 && state.items.len() < self.capacity {
            state.items.push_back(envelope);
            persist_to(&self.path, &state.items)?;
            if durable {
                sync_file_and_parent(&self.path)?;
            }
            return Ok(());
        }
        // Write the line before counting it: a record is spilled only once it
        // is in the tail file, which is what a reopen re-derives the count from.
        match append_spill_line(&self.spill_path, &envelope, state.spill.bytes) {
            Ok(written) => {
                state.spill.count += 1;
                state.spill.bytes += written;
            }
            Err(error) => {
                self.ledger.record(REASON_SPILL_WRITE_FAILED, 1);
                log::warn!(
                    "observability: failed to append to spill tail {} ({error}); dropped 1 \
                     record (spill_write_failed={}, dropped_total={})",
                    self.spill_path.display(),
                    self.ledger
                        .by_reason()
                        .get(REASON_SPILL_WRITE_FAILED)
                        .copied()
                        .unwrap_or(0),
                    self.ledger.total()
                );
                return Err(error);
            }
        }
        if durable {
            sync_file_and_parent(&self.spill_path)?;
        }
        Ok(())
    }

    /// Current queue length: the head window plus every spilled record.
    #[must_use]
    pub fn len(&self) -> usize {
        let state = self.lock();
        state.items.len() + usize::try_from(state.spill.count).unwrap_or(usize::MAX)
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

    /// Clone up to `n` envelopes from the front of the head window **without**
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

    /// Acknowledge only the exported snapshot prefix still present in the FIFO,
    /// then refill the head from the spill tail. Nothing is evicted from the
    /// front, so the head advances only through acks; a replayed (or
    /// overlapping) acknowledgment is idempotent, and records pushed or
    /// refilled while the export was in flight sit at later sequence positions
    /// and are never mistaken for the records it sent.
    pub fn ack_snapshot(&self, snapshot: &QueueSnapshot, acknowledged: usize) {
        let end = snapshot.start_sequence + acknowledged.min(snapshot.envelopes.len()) as u128;
        let mut state = self.lock();
        let count = end
            .saturating_sub(state.head_sequence)
            .min(state.items.len() as u128) as usize;
        state.items.drain(..count);
        state.head_sequence += count as u128;
        self.refill_and_persist(&mut state);
    }

    /// Remove the front `n` envelopes (clamped to the head's length) after a
    /// successful export, refill the head from the spill tail, and persist.
    pub fn ack(&self, n: usize) {
        let mut state = self.lock();
        let n = n.min(state.items.len());
        state.items.drain(0..n);
        state.head_sequence += n as u128;
        self.refill_and_persist(&mut state);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Move spilled records back into the head up to `capacity`, persist the
    /// head, then — only after the head is durable — advance the spill
    /// offset (removing or compacting the tail when due). A crash between the
    /// two writes replays the refilled records from both files: duplicates,
    /// never loss.
    fn refill_and_persist(&self, state: &mut QueueState) {
        let need = self.capacity.saturating_sub(state.items.len());
        let before = state.spill;
        if need > 0 && state.spill.count > 0 {
            match read_spill(&self.spill_path, state.spill.offset, need) {
                Ok(read) => {
                    state.items.extend(read.records);
                    state.spill.offset = read.offset;
                    state.spill.count = if read.eof {
                        // The tail ran out before the count did (the file
                        // shrank under us): resync rather than spin.
                        0
                    } else {
                        state.spill.count.saturating_sub(read.lines)
                    };
                }
                Err(error) => log::warn!(
                    "observability: failed to refill queue head from spill tail {}: {error}",
                    self.spill_path.display()
                ),
            }
        }
        if let Err(error) = persist_to(&self.path, &state.items) {
            log::warn!(
                "observability: failed to persist queue to {}: {error}",
                self.path.display()
            );
            // The head did not reach disk, so the offset must not advance
            // past records that now live only in memory.
            return;
        }
        if before.offset == state.spill.offset && before.count == state.spill.count {
            return;
        }
        if state.spill.count == 0 {
            // Fully drained: truncate both files. Tail first; a stale offset
            // left by a crash here is discarded by `open_spill` because the
            // tail it pointed into is gone.
            remove_if_present(&self.spill_path);
            remove_if_present(&self.offset_path);
            state.spill = SpillState::default();
        } else if state.spill.offset.saturating_mul(2) > state.spill.bytes {
            if let Err(error) = compact_spill(&self.spill_path, &self.offset_path, &mut state.spill)
            {
                log::warn!("observability: failed to compact spill tail: {error}");
                self.write_offset(state.spill.offset);
            }
        } else {
            self.write_offset(state.spill.offset);
        }
    }

    fn write_offset(&self, offset: u64) {
        if let Err(error) = write_offset(&self.offset_path, offset) {
            log::warn!(
                "observability: failed to persist spill offset to {}: {error}",
                self.offset_path.display()
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
    let mut buffer = String::new();
    for item in items {
        if let Ok(line) = serde_json::to_string(item) {
            buffer.push_str(&line);
            buffer.push('\n');
        }
    }
    write_atomic(path, buffer.as_bytes())
}

/// Temp-file-then-rename write of `bytes` to `path`, creating the parent.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".tmp-{}", std::process::id()));
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(temporary, path)
}

fn sync_file_and_parent(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::File::open(parent)?.sync_all()
}

fn remove_if_present(path: &Path) {
    if let Err(error) = std::fs::remove_file(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log::warn!("observability: failed to remove {}: {error}", path.display());
        }
    }
}

fn read_offset(path: &Path) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

fn write_offset(path: &Path, offset: u64) -> std::io::Result<()> {
    write_atomic(path, format!("{offset}\n").as_bytes())
}

fn is_blank(line: &[u8]) -> bool {
    line.iter().all(u8::is_ascii_whitespace)
}

/// Re-derive the spill tail's state from disk: cut off a torn final line,
/// clamp the offset to the tail (moving it back to a line start, which can
/// only replay a record, never skip one), and count the records past it.
/// A missing or empty tail discards any stale offset sidecar.
fn open_spill(spill_path: &Path, offset_path: &Path) -> SpillState {
    let data = std::fs::read(spill_path).unwrap_or_default();
    let valid = data
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    if valid < data.len() {
        log::warn!(
            "observability: cutting a torn {}-byte line off spill tail {}",
            data.len() - valid,
            spill_path.display()
        );
        if let Err(error) = std::fs::OpenOptions::new()
            .write(true)
            .open(spill_path)
            .and_then(|file| file.set_len(valid as u64))
        {
            log::warn!("observability: failed to truncate spill tail: {error}");
        }
    }
    let data = &data[..valid];
    if data.is_empty() {
        remove_if_present(offset_path);
        return SpillState::default();
    }
    let mut offset = usize::try_from(read_offset(offset_path))
        .unwrap_or(usize::MAX)
        .min(data.len());
    while offset > 0 && data[offset - 1] != b'\n' {
        offset -= 1;
    }
    let count = data[offset..]
        .split(|byte| *byte == b'\n')
        .filter(|line| !is_blank(line))
        .count();
    SpillState {
        count: count as u64,
        offset: offset as u64,
        bytes: data.len() as u64,
    }
}

/// Append one JSONL line for `envelope` to the spill tail (`O_APPEND`, no
/// rewrite) and return the bytes written. A failed write is rolled back to
/// `prior_len` so a partial line cannot corrupt the next append.
fn append_spill_line(
    spill_path: &Path,
    envelope: &TelemetryEnvelope,
    prior_len: u64,
) -> std::io::Result<u64> {
    let mut line = serde_json::to_string(envelope).map_err(std::io::Error::other)?;
    line.push('\n');
    if let Some(parent) = spill_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(spill_path)?;
    if let Err(error) = file.write_all(line.as_bytes()) {
        let _ = file.set_len(prior_len);
        return Err(error);
    }
    Ok(line.len() as u64)
}

struct SpillRead {
    records: Vec<TelemetryEnvelope>,
    /// Non-blank lines consumed (parsed or not).
    lines: u64,
    offset: u64,
    eof: bool,
}

/// Read up to `max` records from the spill tail starting at byte `offset`.
fn read_spill(spill_path: &Path, offset: u64, max: usize) -> std::io::Result<SpillRead> {
    let mut file = std::fs::File::open(spill_path)?;
    file.seek(std::io::SeekFrom::Start(offset))?;
    let mut reader = std::io::BufReader::new(file);
    let mut read = SpillRead {
        records: Vec::new(),
        lines: 0,
        offset,
        eof: false,
    };
    let mut line = String::new();
    while read.lines < max as u64 {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 || !line.ends_with('\n') {
            read.eof = true;
            break;
        }
        read.offset += n as u64;
        if is_blank(line.as_bytes()) {
            continue;
        }
        read.lines += 1;
        match serde_json::from_str(&line) {
            Ok(envelope) => read.records.push(envelope),
            Err(error) => log::warn!(
                "observability: skipping unparseable spill line in {}: {error}",
                spill_path.display()
            ),
        }
    }
    Ok(read)
}

/// Rewrite the spill tail from the current offset. The offset is reset to 0
/// **before** the compacted tail replaces the old one: a crash in between
/// replays the already-consumed prefix (duplicates) instead of pointing a
/// stale offset into the shorter file (loss).
fn compact_spill(
    spill_path: &Path,
    offset_path: &Path,
    spill: &mut SpillState,
) -> std::io::Result<()> {
    let data = std::fs::read(spill_path)?;
    let start = usize::try_from(spill.offset)
        .unwrap_or(usize::MAX)
        .min(data.len());
    let tail = &data[start..];
    replace_spill(spill_path, offset_path, tail)?;
    spill.offset = 0;
    spill.bytes = tail.len() as u64;
    Ok(())
}

/// Put `excess` (head records past a shrunk capacity) in front of the
/// unconsumed spill tail, preserving FIFO order.
fn prepend_to_spill(
    spill_path: &Path,
    offset_path: &Path,
    excess: &VecDeque<TelemetryEnvelope>,
    spill: SpillState,
) -> std::io::Result<SpillState> {
    let mut bytes = Vec::new();
    let mut moved = 0u64;
    for envelope in excess {
        if let Ok(line) = serde_json::to_string(envelope) {
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
            moved += 1;
        }
    }
    if spill.bytes > 0 {
        let data = std::fs::read(spill_path)?;
        let start = usize::try_from(spill.offset)
            .unwrap_or(usize::MAX)
            .min(data.len());
        bytes.extend_from_slice(&data[start..]);
    }
    replace_spill(spill_path, offset_path, &bytes)?;
    Ok(SpillState {
        count: spill.count + moved,
        offset: 0,
        bytes: bytes.len() as u64,
    })
}

/// Swap in `contents` as the whole spill tail, read from offset 0. Offset
/// first, then the rename — see [`compact_spill`].
fn replace_spill(spill_path: &Path, offset_path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut temporary = spill_path.as_os_str().to_owned();
    temporary.push(format!(".tmp-{}", std::process::id()));
    let temporary = PathBuf::from(temporary);
    if let Some(parent) = spill_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&temporary, contents)?;
    write_offset(offset_path, 0)?;
    std::fs::rename(temporary, spill_path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::telemetry::{RepoVisibility, SweepStartedRecord, TelemetryRecord};
    use serial_test::serial;
    use tempfile::tempdir;

    fn envelope_with_repo(issue: u32, repo: String) -> TelemetryEnvelope {
        TelemetryEnvelope::new(
            "host-test",
            TelemetryRecord::SweepStarted(SweepStartedRecord {
                story_points: None,
                repo,
                visibility: RepoVisibility::Public,
                issue,
                sweep_id: format!("sweep-issue-{issue}-0"),
                started_at: chrono::Utc::now(),
                facts: Default::default(),
            }),
        )
    }

    fn envelope(issue: u32) -> TelemetryEnvelope {
        envelope_with_repo(issue, "rjwalters/loom".to_string())
    }

    fn issue_of(envelope: &TelemetryEnvelope) -> u32 {
        match &envelope.record {
            TelemetryRecord::SweepStarted(r) => r.issue,
            other => panic!("unexpected record {other:?}"),
        }
    }

    fn issues(envelopes: &[TelemetryEnvelope]) -> Vec<u32> {
        envelopes.iter().map(issue_of).collect()
    }

    /// Drain the whole queue one head-batch at a time, returning issue order.
    fn drain_all(queue: &DurableQueue) -> Vec<u32> {
        let mut out = Vec::new();
        loop {
            let batch = queue.peek_batch(usize::MAX);
            if batch.is_empty() {
                break;
            }
            out.extend(issues(&batch));
            queue.ack(batch.len());
        }
        assert_eq!(queue.len(), 0);
        out
    }

    #[test]
    fn snapshot_ack_survives_spill_refill_and_identical_new_payloads() {
        let dir = tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 2);
        let first = envelope(1);
        let second = envelope(2);
        queue.push(first.clone());
        queue.push(second.clone());
        let snapshot = queue.peek_snapshot(2);
        queue.push(second.clone()); // spills; identical to an in-flight record
        queue.ack_snapshot(&snapshot, 2);
        assert_eq!(queue.peek_batch(2), vec![second.clone()], "refilled from spill");
        queue.ack_snapshot(&snapshot, 2); // replayed acknowledgment is idempotent
        assert_eq!(queue.peek_batch(2), vec![second.clone()]);
        let snapshot = queue.peek_snapshot(1);
        queue.push(first.clone());
        queue.push(second.clone()); // spills behind a full head
        queue.ack_snapshot(&snapshot, 1);
        assert_eq!(queue.peek_batch(2), vec![first, second]);
        assert_eq!(queue.dropped_total(), 0, "nothing is ever evicted");
    }

    #[test]
    fn ack_refill_keeps_fifo_with_pushes_during_an_in_flight_snapshot() {
        let dir = tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 2);
        for i in 1..=3 {
            queue.push(envelope(i));
        }
        let in_flight = queue.peek_snapshot(2);
        assert_eq!(issues(&in_flight.envelopes), vec![1, 2]);
        queue.push(envelope(4)); // pushed while the export is in flight
        queue.ack_snapshot(&in_flight, 2);
        assert_eq!(issues(&queue.peek_batch(10)), vec![3, 4]);
        let in_flight = queue.peek_snapshot(2);
        queue.push(envelope(5));
        queue.push(envelope(6));
        queue.ack_snapshot(&in_flight, 1); // partial ack: only issue 3
        assert_eq!(issues(&queue.peek_batch(10)), vec![4, 5]);
        queue.ack_snapshot(&in_flight, 1); // replay acks nothing new
        assert_eq!(issues(&queue.peek_batch(10)), vec![4, 5]);
        assert_eq!(queue.len(), 3);
        assert_eq!(drain_all(&queue), vec![4, 5, 6]);
        assert_eq!(queue.dropped_total(), 0);
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
        assert_eq!(issues(&batch), vec![1, 2]);
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
        assert_eq!(issues(&queue.peek_batch(10)), vec![3]);
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
    fn overflow_spills_instead_of_dropping_and_drains_in_fifo_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        let queue = DurableQueue::open(path.clone(), 3);
        for i in 1..=10 {
            queue.push(envelope(i));
        }
        assert_eq!(queue.len(), 10, "len counts head + spilled");
        assert_eq!(queue.dropped_total(), 0);
        assert!(queue.dropped_by_reason().is_empty());
        assert_eq!(issues(&queue.peek_batch(usize::MAX)), vec![1, 2, 3], "head window only");
        assert!(spill_path(&path).exists());
        assert_eq!(drain_all(&queue), (1..=10).collect::<Vec<_>>());
        assert_eq!(queue.dropped_total(), 0);
        assert!(!spill_path(&path).exists(), "drained tail is truncated");
        assert!(!spill_offset_path(&path).exists(), "drained offset is truncated");
    }

    #[test]
    fn spilled_tail_is_replayed_after_the_head_across_a_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        {
            let queue = DurableQueue::open(path.clone(), 3);
            for i in 1..=10 {
                queue.push(envelope(i));
            }
        }
        let reopened = DurableQueue::open(path.clone(), 3);
        assert_eq!(reopened.len(), 10);
        assert_eq!(reopened.dropped_total(), 0);
        // Part-drain (refill + offset advance + compaction), then reopen again.
        reopened.ack(2);
        reopened.ack(3);
        drop(reopened);
        let again = DurableQueue::open(path, 3);
        assert_eq!(again.len(), 5);
        assert_eq!(drain_all(&again), (6..=10).collect::<Vec<_>>());
    }

    #[test]
    fn a_lost_offset_replays_duplicates_never_drops() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        {
            let queue = DurableQueue::open(path.clone(), 2);
            for i in 1..=8 {
                queue.push(envelope(i));
            }
            queue.ack(1); // refills issue 3 into the head, offset advances
        }
        // Simulate a crash that lost the offset write after the head write.
        std::fs::remove_file(spill_offset_path(&path)).unwrap();
        let reopened = DurableQueue::open(path, 2);
        let drained = drain_all(&reopened);
        for issue in 2..=8 {
            assert!(drained.contains(&issue), "issue {issue} must survive: {drained:?}");
        }
        assert!(drained.len() > 7, "the refilled record is replayed, not lost");
    }

    #[test]
    fn a_torn_final_spill_line_is_cut_on_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        {
            let queue = DurableQueue::open(path.clone(), 1);
            for i in 1..=3 {
                queue.push(envelope(i));
            }
        }
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(spill_path(&path))
            .unwrap();
        file.write_all(b"{\"torn").unwrap();
        drop(file);
        let reopened = DurableQueue::open(path, 1);
        assert_eq!(reopened.len(), 3);
        reopened.push(envelope(4));
        assert_eq!(drain_all(&reopened), vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_failed_spill_write_is_counted_as_spill_write_failed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        // A directory where the spill file should be: every append fails.
        std::fs::create_dir_all(spill_path(&path)).unwrap();
        let queue = DurableQueue::open(path, 2);
        queue.push(envelope(1));
        queue.push(envelope(2));
        assert_eq!(queue.dropped_total(), 0, "the head window still has room");
        queue.push(envelope(3));
        assert!(queue.push_durable(envelope(4)).is_err(), "a durable spill failure surfaces");
        assert_eq!(queue.len(), 2);
        assert_eq!(
            queue.dropped_by_reason(),
            std::collections::BTreeMap::from([(REASON_SPILL_WRITE_FAILED.to_string(), 2)])
        );
        assert_eq!(queue.dropped_total(), 2);
        // Durable (Issue #11527): a reopen sees the same ledger.
        let reopened = DurableQueue::open(dir.path().join("q.jsonl"), 2);
        assert_eq!(reopened.dropped_by_reason().get("spill_write_failed"), Some(&2));
    }

    #[test]
    fn push_past_the_head_appends_and_never_rewrites_the_spill_or_head() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        let queue = DurableQueue::open(path.clone(), 2);
        queue.push(envelope(1));
        queue.push(envelope(2));
        let head = std::fs::read(&path).unwrap();
        #[cfg(unix)]
        let head_inode = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&path).unwrap().ino()
        };
        queue.push(envelope(3));
        #[cfg(unix)]
        let spill_inode = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(spill_path(&path)).unwrap().ino()
        };
        let mut previous = std::fs::read(spill_path(&path)).unwrap();
        for i in 4..=50 {
            queue.push(envelope(i));
            let current = std::fs::read(spill_path(&path)).unwrap();
            let line = serde_json::to_string(&envelope(i)).unwrap().len() + 1;
            assert!(current.starts_with(&previous), "push {i} rewrote the spill prefix");
            // Same record shape each time, so O(1): one line's worth of bytes.
            assert!(
                current.len() - previous.len() <= line + 8,
                "push {i} wrote {} bytes for a ~{line}-byte line",
                current.len() - previous.len()
            );
            previous = current;
        }
        assert_eq!(std::fs::read(&path).unwrap(), head, "a spilling push leaves the head alone");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(&path).unwrap().ino(), head_inode, "head not renamed");
            assert_eq!(
                std::fs::metadata(spill_path(&path)).unwrap().ino(),
                spill_inode,
                "the spill tail is appended in place, never replaced on push"
            );
        }
        assert_eq!(queue.len(), 50);
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
        assert_eq!(issues(&reopened.peek_batch(10)), vec![1, 2]);
    }

    #[test]
    fn missing_file_opens_as_an_empty_queue() {
        let dir = tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("does-not-exist.jsonl"), 10);
        assert!(queue.is_empty());
        assert_eq!(queue.dropped_total(), 0);
    }

    #[test]
    fn shrunk_capacity_on_reopen_spills_the_excess() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        {
            let queue = DurableQueue::open(path.clone(), 10);
            for i in 1..=5 {
                queue.push(envelope(i));
            }
        }
        {
            let reopened = DurableQueue::open(path.clone(), 2);
            assert_eq!(reopened.len(), 5, "nothing trimmed");
            assert_eq!(issues(&reopened.peek_batch(10)), vec![1, 2]);
            assert_eq!(reopened.dropped_total(), 0);
            assert!(reopened.dropped_by_reason().is_empty());
            reopened.push(envelope(6)); // behind the moved excess
        }
        let reopened_again = DurableQueue::open(path.clone(), 2);
        assert_eq!(reopened_again.len(), 6, "no double move on a second reopen");
        drop(reopened_again);
        // An older, still-spilled tail stays behind the newly moved excess.
        let shrunk_more = DurableQueue::open(path, 1);
        assert_eq!(drain_all(&shrunk_more), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn grown_capacity_on_reopen_refills_the_head() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        {
            let queue = DurableQueue::open(path.clone(), 2);
            for i in 1..=5 {
                queue.push(envelope(i));
            }
        }
        let reopened = DurableQueue::open(path.clone(), 10);
        assert_eq!(issues(&reopened.peek_batch(10)), vec![1, 2, 3, 4, 5]);
        assert!(!spill_path(&path).exists());
    }

    #[tokio::test]
    async fn refills_from_spill_still_flush_in_byte_bounded_batches() {
        use super::super::exporter::{ExportError, Exporter};
        use super::super::sender::{json_len, try_flush, FlushOutcome, MAX_BATCH_BYTES};

        struct Recording(Mutex<Vec<Vec<TelemetryEnvelope>>>);
        impl Exporter for Recording {
            async fn emit_batch(&self, batch: &[TelemetryEnvelope]) -> Result<(), ExportError> {
                self.0.lock().unwrap().push(batch.to_vec());
                Ok(())
            }
        }

        let dir = tempdir().unwrap();
        let queue = DurableQueue::open(dir.path().join("q.jsonl"), 4);
        // ~1.5 MiB each: a full 4-record head is ~6 MiB, over the 4 MiB budget.
        for i in 1..=7 {
            queue.push(envelope_with_repo(i, "x".repeat(1_500_000)));
        }
        assert_eq!(queue.len(), 7);
        let exporter = Recording(Mutex::new(Vec::new()));
        let status =
            super::super::ExportStatus::started("host-test", "https://x.invalid", "https", 30);
        while !queue.is_empty() {
            assert!(matches!(
                try_flush(&queue, &exporter, 100, &status).await,
                FlushOutcome::Sent(_)
            ));
        }
        let batches = exporter.0.into_inner().unwrap();
        assert!(batches.len() > 2, "refilled head was split by bytes");
        for batch in &batches {
            assert!(json_len(batch) <= MAX_BATCH_BYTES, "batch over the byte budget");
        }
        let sent: Vec<u32> = batches.iter().flat_map(|b| issues(b.as_slice())).collect();
        assert_eq!(sent, (1..=7).collect::<Vec<_>>());
        assert_eq!(queue.dropped_total(), 0);
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
        // A directory as the queue file path forces the persist to fail.
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
