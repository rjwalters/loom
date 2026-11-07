//! Journal → observability export queue (no new transport).
//!
//! The local journal is the export queue-of-record, exactly like
//! `sweep-outcome-telemetry.jsonl` ([`crate::observability::backfill`]):
//! the observability backfill pass calls [`backfill`], which offers every
//! journal line past a persisted byte cursor to the configured exporter
//! queue(s) and advances the cursor. Only complete lines are consumed, so a
//! concurrent writer's in-flight append is never half-read.
//!
//! **Bounded memory (#11045).** A pass seeks to the cursor and streams one
//! line at a time; nothing here reads the journal whole (a 5.4 GB journal
//! read whole OOM-killed the fleet captain). A pass holds the per-host cycle
//! lock, so it never overlaps a poll cycle, and it is the one place the
//! journal is rotated ([`super::rotation`]) — once the bulk cursor passes the
//! threshold, carrying any unexported tail into the new generation (#13636).
//!
//! Delivery to the exporter is at-least-once across a crash between the
//! queue offer and the cursor save (the same posture as every other
//! backfill); every CI record carries stable GitHub/trace identities, so a
//! re-offered record is recognisable downstream.
//!
//! **Priority lane (#12792).** The journal is dominated ~100:1 by per-test /
//! step / suite spans, so a strict FIFO cursor leaves the fresh `ci.run` /
//! `ci.job` facts (what ETA consumes) queued behind millions of older spans
//! whenever the backfill drain is capped. [`ExportCursor::fast_offset`] is a
//! second persisted cursor over the *same* journal: each pass first offers the
//! run/job-class lines ([`is_priority`]) past it, then continues the bulk
//! cursor, which skips the priority lines the fast lane already offered. A
//! fast lane that stops early (scan budget, queue push-back) bounds the bulk
//! lane to the span it already covered instead of starving it (#13685). Nothing is dropped
//! or sampled; rotation does not wait for EOF (a live writer keeps the bulk
//! cursor trailing it): the unexported tail is carried forward and the fast
//! offset resets, so priority lines in the tail may be re-offered once.
//!
//! **Bounded admission (#11115).** Records are offered through
//! [`QueueSink::offer_backfill`], which never evicts: when any exporter queue
//! is at its backfill limit the pass stops at that line and leaves it (and
//! everything after it) behind the cursor for the next pass. A fan-out offer
//! accepted by one exporter but deferred by another is re-offered later, so
//! the already-accepting exporter sees a duplicate (at-least-once), never a
//! silent omission.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::journal::MAX_LINE_BYTES;
use super::ledger::{complete_len, scan_lines_at};
use super::rotation::{self, RotationPolicy};
use super::state::CycleLock;
use super::{journal_path, read_config, state_dir};
use crate::observability::queue::{Admission, QueueSink};
use crate::telemetry::TelemetryEnvelope;

/// Most journal bytes the fast lane reads in one pass. A fast lane catching up
/// over a multi-GB span backlog (it only offers a sliver of what it reads)
/// stays bounded in time under the cycle lock and resumes next pass.
const FAST_LANE_SCAN_BYTES: u64 = 256 * 1024 * 1024;

/// Persisted progress through the journal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportCursor {
    /// Journal byte offset of the first line not yet offered by the bulk
    /// lane (every line before it has been offered, whatever its kind).
    pub byte_offset: u64,
    /// Priority-lane progress: every run/job-class line before this offset
    /// has been offered. `None` (also what a pre-#12792 cursor file reads
    /// as) means "same as `byte_offset`", so an upgrade starts the fast lane
    /// at the bulk cursor — not at `0` and not at EOF.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_offset: Option<u64>,
    /// Envelopes offered to the export queue so far (cumulative, across
    /// rotations).
    pub exported: u64,
}

fn cursor_path(root: &Path) -> PathBuf {
    state_dir(root).join("export-cursor.json")
}

#[must_use]
pub fn load_cursor(root: &Path) -> ExportCursor {
    File::open(cursor_path(root))
        .ok()
        .and_then(|file| serde_json::from_reader(io::BufReader::new(file)).ok())
        .unwrap_or_default()
}

/// One step of the durable cursor save, in the order they run. Exposed so
/// tests can inject a failure at exactly one boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaveStep {
    CreateDir,
    WriteTemp,
    SyncFile,
    Rename,
    SyncDir,
}

/// Durably persist `cursor`: write a temporary file, fsync it, atomically
/// rename it over the cursor, then fsync the parent directory. Every failure
/// is returned; on error the previous cursor file (if any) is untouched and a
/// leftover temporary is removed.
fn save_cursor(root: &Path, cursor: &ExportCursor) -> io::Result<()> {
    save_cursor_with(root, cursor, &|_| Ok(()))
}

/// [`save_cursor`] with a per-step fault hook (`fail(step)` returning `Err`
/// aborts at that step, before the real operation runs).
pub(crate) fn save_cursor_with(
    root: &Path,
    cursor: &ExportCursor,
    fail: &dyn Fn(SaveStep) -> io::Result<()>,
) -> io::Result<()> {
    use std::io::Write;

    let path = cursor_path(root);
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    fail(SaveStep::CreateDir)?;
    std::fs::create_dir_all(&dir)?;
    let text = serde_json::to_string(cursor).map_err(io::Error::other)?;
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let staged = (|| {
        fail(SaveStep::WriteTemp)?;
        let mut file = File::create(&temporary)?;
        file.write_all(text.as_bytes())?;
        fail(SaveStep::SyncFile)?;
        file.sync_all()?;
        drop(file);
        fail(SaveStep::Rename)?;
        std::fs::rename(&temporary, &path)
    })();
    if let Err(error) = staged {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    fail(SaveStep::SyncDir)?;
    File::open(&dir)?.sync_all()
}

/// Where an export pass starts: the bulk and fast cursors, or `0` for both
/// when the journal's complete prefix is shorter than the bulk cursor (the
/// journal was replaced, rotated, or deleted and recreated). The fast offset
/// is clamped into `[bulk, complete]`. Reads only the journal's tail. `None`
/// when the journal is missing.
fn start_offsets(path: &Path, cursor: &ExportCursor) -> Option<(u64, u64)> {
    let mut file = File::open(path).ok()?;
    let complete = complete_len(&mut file).ok()?;
    if cursor.byte_offset > complete {
        return Some((0, 0));
    }
    let bulk = cursor.byte_offset;
    Some((bulk, cursor.fast_offset.unwrap_or(0).clamp(bulk, complete)))
}

/// The `fast_offset` to persist for a pass that ended with the bulk cursor at
/// `bulk` and the fast lane at `fast`: `None` once the lanes coincide.
fn normalized_fast(bulk: u64, fast: u64) -> Option<u64> {
    (fast > bulk).then_some(fast)
}

/// Just enough of a journal line to classify it without deserialising the
/// (potentially large) record body.
#[derive(Deserialize)]
struct LineHead {
    record: RecordHead,
}

#[derive(Deserialize)]
struct RecordHead {
    kind: String,
    #[serde(default, deserialize_with = "lenient_name")]
    name: Option<String>,
}

fn lenient_name<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(name) => Some(name),
        _ => None,
    })
}

/// Whether a journal line belongs to the priority lane: the `ci.run` /
/// `ci.job` facts, their `ci.duration` metric carriers, and the
/// `loom.ci.run` / `loom.ci.job` spans. Everything else (step / suite / test
/// spans, job-log chunks) is bulk. An unclassifiable line is bulk.
pub(crate) fn is_priority(line: &[u8]) -> bool {
    let Ok(head) = serde_json::from_slice::<LineHead>(line) else {
        return false;
    };
    match head.record.kind.as_str() {
        "ci.run" | "ci.job" | "ci.duration" => true,
        "trace.span" => matches!(head.record.name.as_deref(), Some("loom.ci.run" | "loom.ci.job")),
        _ => false,
    }
}

/// Journal envelopes not yet offered for export, across both lanes. Streams
/// only the bytes past the bulk cursor (#11045).
#[must_use]
pub fn pending_count(root: &Path) -> usize {
    let path = journal_path(root);
    let Some((start, fast)) = start_offsets(&path, &load_cursor(root)) else {
        return 0;
    };
    let mut count = 0;
    let _ = scan_lines_at(&path, start, MAX_LINE_BYTES, |offset, line| {
        if !line.trim_ascii().is_empty() && !(offset < fast && is_priority(line)) {
            count += 1;
        }
        true
    });
    count
}

/// Offer every not-yet-exported journal envelope to `queue`, then rotate the
/// journal if it is due ([`rotation`]). Returns how many were offered. A
/// journal shorter than the cursor (replaced or deleted and recreated)
/// restarts from the beginning of the new file.
///
/// Runs under the per-host cycle lock (`poll.lock`, #11045), so an export
/// pass never overlaps a poll cycle's journal work and a rotation can never
/// race an append. When a cycle holds the lock the pass is skipped (the next
/// backfill tick retries). Reads only the bytes past the cursor, one line at
/// a time.
pub fn backfill(root: &Path, queue: &dyn QueueSink) -> usize {
    let path = journal_path(root);
    if !path.exists() && !rotation::carry_path(&path).exists() {
        // Nothing to export — and no lock file is created in a workspace
        // that has never run the poller. (A lone staging file is an
        // interrupted tail-carrying rotation: recovered below.)
        return 0;
    }
    let lock = match CycleLock::try_acquire(&state_dir(root)) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            log::debug!("ci_telemetry: export skipped — a poll cycle holds the lock");
            return 0;
        }
        Err(error) => {
            log::warn!("ci_telemetry: export skipped — could not take the cycle lock: {error}");
            return 0;
        }
    };
    // Finish an interrupted tail-carrying rotation before reading the journal
    // (#13636): otherwise the carried tail never reaches the exporter.
    match rotation::recover_carry(&path) {
        Ok(true) => log::info!("ci_telemetry: recovered an interrupted rotation's carried tail"),
        Ok(false) => {}
        Err(error) => {
            log::warn!("ci_telemetry: could not recover the carried tail, will retry: {error}");
            return 0;
        }
    }
    if !path.exists() {
        return 0;
    }
    let (offered, mut cursor) = export_locked(root, &path, queue);
    let policy = if cursor.byte_offset > 0 {
        RotationPolicy::resolve(&read_config(root))
    } else {
        RotationPolicy::default()
    };
    if let Err(error) = rotate_if_due(root, &path, &mut cursor, policy) {
        log::warn!("ci_telemetry: journal rotation failed, will retry: {error}");
    }
    drop(lock);
    offered
}

/// How one journal line fared against the export queue.
enum Offer {
    Admitted,
    /// Unparseable: logged and passed over.
    Skipped,
    /// The queue deferred or refused it: the lane stops at this line.
    Stop(StopReason),
}

/// Why an export lane stopped before the end of the journal (#13685).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    /// The per-pass scan budget ([`FAST_LANE_SCAN_BYTES`]) ran out.
    Budget,
    /// An exporter queue is at its backfill limit (#11115).
    Deferred,
    /// An exporter queue refused the record.
    Refused,
}

impl StopReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Budget => "scan budget exhausted",
            Self::Deferred => "exporter queue deferred (backfill limit)",
            Self::Refused => "exporter queue refused a record",
        }
    }
}

fn offer_line(queue: &dyn QueueSink, line: &[u8]) -> Offer {
    let text = String::from_utf8_lossy(line);
    let envelope = match serde_json::from_str::<TelemetryEnvelope>(text.trim()) {
        Ok(envelope) => envelope,
        Err(error) => {
            log::warn!("ci_telemetry: skipping unparseable journal line: {error}");
            return Offer::Skipped;
        }
    };
    match queue.offer_backfill(envelope) {
        Ok(Admission::Admitted) => Offer::Admitted,
        Ok(Admission::Deferred) => {
            // #11115: stop at the first unadmitted record; it stays behind
            // the cursor and no work is spent on the rest of the journal.
            log::debug!("ci_telemetry: export deferred — exporter queue at its backfill limit");
            Offer::Stop(StopReason::Deferred)
        }
        Err(error) => {
            log::warn!("ci_telemetry: export queue refused a record, will retry: {error}");
            Offer::Stop(StopReason::Refused)
        }
    }
}

/// The export pass proper. The caller holds the cycle lock.
///
/// Fast lane first (priority lines past `fast_offset`, byte-bounded), then the
/// bulk lane over everything past `byte_offset`, skipping priority lines the
/// fast lane already offered. If the fast lane reached the end of the journal
/// the bulk lane runs to the end too; if the fast lane stopped early (scan
/// budget, queue push-back) the bulk lane is bounded to the span the fast lane
/// already covered, so it still advances rather than starving behind a fast
/// lane that cannot finish (#13685) — rotation keys off the bulk cursor.
fn export_locked(root: &Path, path: &Path, queue: &dyn QueueSink) -> (usize, ExportCursor) {
    let cursor = load_cursor(root);
    let Some((bulk_start, fast_start)) = start_offsets(path, &cursor) else {
        return (0, cursor);
    };
    let mut offered = 0;

    // Fast lane.
    let mut read = 0_u64;
    let mut pushed_back: Option<StopReason> = None;
    let fast_scan = scan_lines_at(path, fast_start, MAX_LINE_BYTES, |_, line| {
        if read >= FAST_LANE_SCAN_BYTES {
            pushed_back = Some(StopReason::Budget); // not at the end yet
            return false;
        }
        read += line.len() as u64 + 1;
        if line.trim_ascii().is_empty() || !is_priority(line) {
            return true;
        }
        match offer_line(queue, line) {
            Offer::Admitted => {
                offered += 1;
                true
            }
            Offer::Skipped => true,
            Offer::Stop(reason) => {
                pushed_back = Some(reason);
                false
            }
        }
    });
    let fast_end = match fast_scan {
        Ok(offset) => offset,
        Err(error) => {
            log::warn!("ci_telemetry: journal export read failed, will retry: {error}");
            return (0, cursor);
        }
    };
    if let Some(reason) = pushed_back {
        log::info!(
            "ci_telemetry: export fast lane stopped at offset {fast_end} of {}: {} (bulk cursor {bulk_start})",
            path.display(),
            reason.as_str()
        );
    }

    // Bulk lane. Every priority line before `fast_end` is already offered. When
    // the fast lane stopped early the bulk lane may not pass `fast_end`: lines
    // beyond it are the fast lane's to classify.
    let bulk_limit = pushed_back.map(|_| fast_end);
    let mut bulk_stop = None;
    let bulk_scan = scan_lines_at(path, bulk_start, MAX_LINE_BYTES, |offset, line| {
        if bulk_limit.is_some_and(|limit| offset >= limit) {
            return false;
        }
        if line.trim_ascii().is_empty() || (offset < fast_end && is_priority(line)) {
            return true;
        }
        match offer_line(queue, line) {
            Offer::Admitted => {
                offered += 1;
                true
            }
            Offer::Skipped => true,
            Offer::Stop(reason) => {
                bulk_stop = Some(reason);
                false
            }
        }
    });
    let bulk_end = match bulk_scan {
        Ok(offset) => offset,
        Err(error) => {
            log::warn!("ci_telemetry: journal export read failed, will retry: {error}");
            // The fast lane's offers were not saved: keep the old cursor and
            // re-offer next pass (at-least-once).
            return (0, cursor);
        }
    };
    if let Some(reason) = bulk_stop {
        log::info!(
            "ci_telemetry: export bulk lane stopped at offset {bulk_end}: {}",
            reason.as_str()
        );
    }
    let advanced = ExportCursor {
        byte_offset: bulk_end,
        fast_offset: normalized_fast(bulk_end, fast_end.max(bulk_end)),
        exported: cursor.exported + offered as u64,
    };
    (offered, persist(root, cursor, advanced))
}

/// Durably save `advanced` when it differs from `cursor`; on a failed save keep
/// (and return) the old cursor so nothing rotates on it and the offers are
/// re-made next pass (at-least-once).
fn persist(root: &Path, cursor: ExportCursor, advanced: ExportCursor) -> ExportCursor {
    if advanced.byte_offset == cursor.byte_offset && advanced.fast_offset == cursor.fast_offset {
        return cursor;
    }
    match save_cursor(root, &advanced) {
        Ok(()) => advanced,
        Err(error) => {
            log::warn!("ci_telemetry: could not persist export cursor, will retry: {error}");
            cursor
        }
    }
}

/// Rotate the journal when the bulk cursor has passed `policy`'s threshold.
/// Any unexported tail `[byte_offset, EOF)` is copied into the new generation
/// first (#13636), so a live writer cannot postpone rotation forever. Then
/// reset the cursor's byte offset. The caller holds the cycle lock.
/// Returns whether it rotated.
pub fn rotate_if_due(
    root: &Path,
    path: &Path,
    cursor: &mut ExportCursor,
    policy: RotationPolicy,
) -> io::Result<bool> {
    rotate_if_due_with(root, path, cursor, policy, &|_| Ok(()))
}

/// [`rotate_if_due`] with a fault hook for the pre-rotation cursor save.
pub(crate) fn rotate_if_due_with(
    root: &Path,
    path: &Path,
    cursor: &mut ExportCursor,
    policy: RotationPolicy,
    save_hook: &dyn Fn(SaveStep) -> io::Result<()>,
) -> io::Result<bool> {
    if cursor.byte_offset == 0 || cursor.byte_offset < policy.threshold_bytes {
        // Nothing consumed yet (e.g. the fast lane is still pushed back):
        // rotating would just copy the whole journal.
        return Ok(false);
    }
    let len = std::fs::metadata(path)?.len();
    let tail_len = len.saturating_sub(cursor.byte_offset);
    if tail_len > 0 && policy.keep == 0 {
        // No retained rotation to replay the tail from after a crash: wait
        // for it to drain.
        return Ok(false);
    }
    // Stage the unexported tail (fsynced) before anything is changed.
    let staged = if tail_len > 0 {
        Some(rotation::carry_tail(path, cursor.byte_offset)?)
    } else {
        None
    };
    // Durably reset the cursor to 0 BEFORE the journal is renamed/deleted, so
    // a crash anywhere afterwards can never leave the old offset trusted for a
    // new generation. A crash before the rotation merely replays the old
    // generation (at-least-once); one between the renames leaves the tail in
    // `.1`, which recovery replay scans. If the reset cannot be made durable,
    // do not rotate.
    let reset = ExportCursor {
        byte_offset: 0,
        fast_offset: None,
        exported: cursor.exported,
    };
    let rotated = save_cursor_with(root, &reset, save_hook).and_then(|()| match &staged {
        Some(tail) => rotation::rotate_carrying(path, policy.keep, tail),
        None => rotation::rotate(path, policy.keep),
    });
    if let Err(error) = rotated {
        if let Some(tail) = &staged {
            // Only drop the staging file if the journal still exists; after a
            // partial rotate it may be the sole copy of the new generation.
            if path.exists() {
                let _ = std::fs::remove_file(tail);
            }
        }
        return Err(error);
    }
    log::info!(
        "ci_telemetry: rotated the {len}-byte journal carrying {tail_len} unexported byte(s) (threshold {} bytes, keeping {} rotation(s))",
        policy.threshold_bytes,
        policy.keep
    );
    cursor.byte_offset = 0;
    cursor.fast_offset = None;
    Ok(true)
}
