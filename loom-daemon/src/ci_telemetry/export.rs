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

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::journal::MAX_LINE_BYTES;
use super::ledger::{complete_len, scan_lines};
use super::rotation::{self, RotationPolicy};
use super::state::CycleLock;
use super::{journal_path, read_config, state_dir};
use crate::observability::queue::QueueSink;
use crate::telemetry::TelemetryEnvelope;

/// Persisted progress through the journal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportCursor {
    /// Journal byte offset of the first line not yet offered.
    pub byte_offset: u64,
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

fn save_cursor(root: &Path, cursor: &ExportCursor) {
    let path = cursor_path(root);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(text) = serde_json::to_string(cursor) else {
        return;
    };
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    if std::fs::write(&temporary, text).is_ok() {
        if let Err(error) = std::fs::rename(&temporary, &path) {
            log::warn!("ci_telemetry: could not persist export cursor: {error}");
        }
    }
}

/// Where an export pass starts: the cursor, or `0` when the journal's
/// complete prefix is shorter than the cursor (the journal was replaced,
/// rotated, or deleted and recreated). Reads only the journal's tail.
/// `None` when the journal is missing.
fn start_offset(path: &Path, cursor: &ExportCursor) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let complete = complete_len(&mut file).ok()?;
    Some(if cursor.byte_offset > complete {
        0
    } else {
        cursor.byte_offset
    })
}

/// Journal envelopes not yet offered for export. Streams only the bytes
/// past the cursor (#11045).
#[must_use]
pub fn pending_count(root: &Path) -> usize {
    let path = journal_path(root);
    let Some(start) = start_offset(&path, &load_cursor(root)) else {
        return 0;
    };
    let mut count = 0;
    let _ = scan_lines(&path, start, MAX_LINE_BYTES, |line| {
        if !line.trim_ascii().is_empty() {
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

/// The export pass proper. The caller holds the cycle lock.
fn export_locked(root: &Path, path: &Path, queue: &dyn QueueSink) -> (usize, ExportCursor) {
    let mut cursor = load_cursor(root);
    let Some(start) = start_offset(path, &cursor) else {
        return (0, cursor);
    };
    let mut offered = 0;
    let scanned = scan_lines(path, start, MAX_LINE_BYTES, |line| {
        let text = String::from_utf8_lossy(line);
        let text = text.trim();
        if text.is_empty() {
            return true;
        }
        match serde_json::from_str::<TelemetryEnvelope>(text) {
            Ok(envelope) => {
                if let Err(error) = queue.offer_durable(envelope) {
                    log::warn!("ci_telemetry: export queue refused a record, will retry: {error}");
                    return false;
                }
                offered += 1;
            }
            Err(error) => log::warn!("ci_telemetry: skipping unparseable journal line: {error}"),
        }
        true
    });
    let offset = match scanned {
        Ok(offset) => offset,
        Err(error) => {
            log::warn!("ci_telemetry: journal export read failed, will retry: {error}");
            return (0, cursor);
        }
    };
    if offset != cursor.byte_offset {
        cursor.byte_offset = offset;
        cursor.exported += offered as u64;
        save_cursor(root, &cursor);
    }
    (offered, cursor)
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
    if cursor.byte_offset < policy.threshold_bytes {
        return Ok(false);
    }
    let len = std::fs::metadata(path)?.len();
    if cursor.byte_offset != len {
        // Unexported lines (or a torn tail a writer has yet to repair)
        // remain; a later pass rotates once they are gone.
        return Ok(false);
    }
    rotation::rotate(path, policy.keep)?;
    log::info!(
        "ci_telemetry: rotated the {len}-byte journal (threshold {} bytes, keeping {} rotation(s))",
        policy.threshold_bytes,
        policy.keep
    );
    cursor.byte_offset = 0;
    save_cursor(root, cursor);
    Ok(true)
}
