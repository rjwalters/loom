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
//! journal is rotated ([`super::rotation`]) — only once every line is
//! exported.
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
//! run/job-class lines ([`is_priority`]) past it, then — only once the fast
//! lane has reached the end of the journal — continues the bulk cursor, which
//! skips the priority lines the fast lane already offered. Nothing is dropped
//! or sampled; the journal rotates only when the bulk cursor (and so the fast
//! one) is at EOF.
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
    if !path.exists() {
        // Nothing to export — and no lock file is created in a workspace
        // that has never run the poller.
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
    Stop,
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
            Offer::Stop
        }
        Err(error) => {
            log::warn!("ci_telemetry: export queue refused a record, will retry: {error}");
            Offer::Stop
        }
    }
}

/// The export pass proper. The caller holds the cycle lock.
///
/// Fast lane first (priority lines past `fast_offset`, byte-bounded), then —
/// only if the fast lane reached the end of the journal without the queue
/// pushing back — the bulk lane over everything past `byte_offset`, skipping
/// priority lines the fast lane already offered.
fn export_locked(root: &Path, path: &Path, queue: &dyn QueueSink) -> (usize, ExportCursor) {
    let cursor = load_cursor(root);
    let Some((bulk_start, fast_start)) = start_offsets(path, &cursor) else {
        return (0, cursor);
    };
    let mut offered = 0;

    // Fast lane.
    let mut read = 0_u64;
    let mut pushed_back = false;
    let fast_scan = scan_lines_at(path, fast_start, MAX_LINE_BYTES, |_, line| {
        if read >= FAST_LANE_SCAN_BYTES {
            pushed_back = true; // out of budget, not at the end yet
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
            Offer::Stop => {
                pushed_back = true;
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
    if pushed_back {
        // Persist what the fast lane did and leave the bulk lane for later.
        let advanced = ExportCursor {
            byte_offset: bulk_start,
            fast_offset: normalized_fast(bulk_start, fast_end),
            exported: cursor.exported + offered as u64,
        };
        return (offered, persist(root, cursor, advanced));
    }

    // Bulk lane. The fast lane is at EOF, so every priority line before
    // `fast_end` is already offered.
    let bulk_scan = scan_lines_at(path, bulk_start, MAX_LINE_BYTES, |offset, line| {
        if line.trim_ascii().is_empty() || (offset < fast_end && is_priority(line)) {
            return true;
        }
        match offer_line(queue, line) {
            Offer::Admitted => {
                offered += 1;
                true
            }
            Offer::Skipped => true,
            Offer::Stop => false,
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

/// Rotate the journal when the cursor has passed `policy`'s threshold **and**
/// sits exactly at the end of the file, so every line is already exported.
/// Then reset the cursor's byte offset. The caller holds the cycle lock.
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
    if cursor.byte_offset < policy.threshold_bytes {
        return Ok(false);
    }
    let len = std::fs::metadata(path)?.len();
    if cursor.byte_offset != len {
        // Unexported lines (or a torn tail a writer has yet to repair)
        // remain; a later pass rotates once they are gone.
        return Ok(false);
    }
    // Durably reset the cursor to 0 BEFORE the journal is renamed/deleted, so
    // a crash anywhere afterwards can never leave the old offset trusted for a
    // new generation. A crash before the rotation merely replays the old,
    // already-offered generation (at-least-once). If the reset cannot be made
    // durable, do not rotate.
    let reset = ExportCursor {
        byte_offset: 0,
        fast_offset: None,
        exported: cursor.exported,
    };
    save_cursor_with(root, &reset, save_hook)?;
    rotation::rotate(path, policy.keep)?;
    log::info!(
        "ci_telemetry: rotated the {len}-byte journal (threshold {} bytes, keeping {} rotation(s))",
        policy.threshold_bytes,
        policy.keep
    );
    cursor.byte_offset = 0;
    cursor.fast_offset = None;
    Ok(true)
}
