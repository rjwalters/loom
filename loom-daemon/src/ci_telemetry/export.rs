//! Journal → observability export queue (no new transport).
//!
//! The local journal is the export queue-of-record, exactly like
//! `sweep-outcome-telemetry.jsonl` ([`crate::observability::backfill`]):
//! the observability backfill pass calls [`backfill`], which offers every
//! journal line past a persisted byte cursor to the configured exporter
//! queue(s) and advances the cursor. Only complete lines are consumed, so a
//! concurrent writer's in-flight append is never half-read.
//!
//! Delivery to the exporter is at-least-once across a crash between the
//! queue offer and the cursor save (the same posture as every other
//! backfill); every CI record carries stable GitHub/trace identities, so a
//! re-offered record is recognisable downstream.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::ledger::complete_prefix_len;
use super::{journal_path, state_dir};
use crate::observability::queue::QueueSink;
use crate::telemetry::TelemetryEnvelope;

/// Persisted progress through the journal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportCursor {
    /// Journal byte offset of the first line not yet offered.
    pub byte_offset: u64,
    /// Envelopes offered to the export queue so far (cumulative).
    pub exported: u64,
}

fn cursor_path(root: &Path) -> PathBuf {
    state_dir(root).join("export-cursor.json")
}

#[must_use]
pub fn load_cursor(root: &Path) -> ExportCursor {
    std::fs::read_to_string(cursor_path(root))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
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

/// Journal envelopes not yet offered for export.
#[must_use]
pub fn pending_count(root: &Path) -> usize {
    let Ok(bytes) = std::fs::read(journal_path(root)) else {
        return 0;
    };
    let complete = complete_prefix_len(&bytes);
    let offset = usize::try_from(load_cursor(root).byte_offset).unwrap_or(usize::MAX);
    let start = if offset > complete { 0 } else { offset };
    bytes[start..complete]
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .count()
}

/// Offer every not-yet-exported journal envelope to `queue`. Returns how
/// many were offered. A journal shorter than the cursor (replaced or
/// deleted and recreated) restarts from the beginning of the new file.
pub fn backfill(root: &Path, queue: &dyn QueueSink) -> usize {
    let Ok(bytes) = std::fs::read(journal_path(root)) else {
        return 0;
    };
    let complete = complete_prefix_len(&bytes);
    let mut cursor = load_cursor(root);
    if usize::try_from(cursor.byte_offset).map_or(true, |offset| offset > complete) {
        cursor.byte_offset = 0;
    }
    let start = usize::try_from(cursor.byte_offset).unwrap_or(0);
    let mut offset = start;
    let mut offered = 0;
    for line in bytes[start..complete].split_inclusive(|b| *b == b'\n') {
        let text = String::from_utf8_lossy(line);
        if !text.trim().is_empty() {
            match serde_json::from_str::<TelemetryEnvelope>(text.trim()) {
                Ok(envelope) => {
                    if let Err(error) = queue.offer_durable(envelope) {
                        log::warn!(
                            "ci_telemetry: export queue refused a record, will retry: {error}"
                        );
                        break;
                    }
                    offered += 1;
                }
                Err(error) => {
                    log::warn!("ci_telemetry: skipping unparseable journal line: {error}")
                }
            }
        }
        offset += line.len();
    }
    if offset != start {
        cursor.byte_offset = offset as u64;
        cursor.exported += offered as u64;
        save_cursor(root, &cursor);
    }
    offered
}
