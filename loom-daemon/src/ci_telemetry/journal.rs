//! The local CI telemetry journal — `.loom/logs/ci-telemetry.jsonl`.
//!
//! One [`TelemetryEnvelope`] per line, written **regardless of exporter
//! configuration** (the `sweep-outcome-telemetry.jsonl` pattern), so the
//! poller is useful offline and the journal is the export queue-of-record
//! ([`super::export::backfill`] drains it into the observability queue).

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

use super::ledger::{append_durable, repair_torn_tail, scan_lines};
use super::records::envelope_identity;
use super::rotation::existing_rotations;

/// Longest journal line a scan will buffer. One line is one envelope (a
/// `ci.job.log` chunk is at most 8 KiB), so anything near this is corrupt;
/// it is skipped rather than allowed to pull the file into memory.
pub const MAX_LINE_BYTES: u64 = 64 * 1024 * 1024;

/// Counts of the journal's contents, for `status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalCounts {
    pub runs: usize,
    pub jobs: usize,
    /// `ci.job.log` chunk records (#8825).
    pub job_log_chunks: usize,
    pub envelopes: usize,
}

/// Append-only journal handle.
///
/// **No path here loads the journal** (#11045): opening repairs only the
/// tail, and every read streams one line at a time, so memory never scales
/// with the journal's size. A source-scan test
/// (`tests/ci_telemetry_journal_streaming.rs`) keeps it that way.
#[derive(Debug, Clone)]
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    /// Open the journal **as its writer** (under the cycle lock), repairing
    /// a torn tail left by a crash mid-append. Reads only the tail.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        repair_torn_tail(&path)?;
        Ok(Journal { path })
    }

    /// Open the journal read-only (never repairs — safe beside a writer).
    #[must_use]
    pub fn reader(path: PathBuf) -> Self {
        Journal { path }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stream every parseable envelope of `path` to `visit`, which returns
    /// `false` to stop early.
    fn scan(path: &Path, mut visit: impl FnMut(TelemetryEnvelope) -> bool) -> io::Result<()> {
        scan_lines(path, 0, MAX_LINE_BYTES, |line| {
            match serde_json::from_slice::<TelemetryEnvelope>(line) {
                Ok(envelope) => visit(envelope),
                Err(_) => true,
            }
        })?;
        Ok(())
    }

    /// Visit every envelope currently in the journal (unparseable lines
    /// skipped), streaming.
    pub fn for_each(&self, mut visit: impl FnMut(TelemetryEnvelope)) -> io::Result<()> {
        Self::scan(&self.path, |envelope| {
            visit(envelope);
            true
        })
    }

    /// Every envelope currently in the journal (unparseable lines skipped).
    /// Holds them all — for tests and small journals; production paths use
    /// [`Self::for_each`] or [`Self::missing`].
    pub fn read_all(&self) -> io::Result<Vec<TelemetryEnvelope>> {
        let mut all = Vec::new();
        self.for_each(|envelope| all.push(envelope))?;
        Ok(all)
    }

    /// The envelopes of `wanted` the journal does not already hold — the
    /// "already emitted" test a pending-unit replay uses. Envelopes with no
    /// identity are always missing.
    ///
    /// Streams the journal, then its retained rotations (a crash can leave a
    /// pending unit whose lines were journaled just before a rotation), and
    /// stops as soon as every identity is found. Memory is bounded by
    /// `wanted`, not by the journal.
    pub fn missing(&self, wanted: &[TelemetryEnvelope]) -> io::Result<Vec<TelemetryEnvelope>> {
        let mut unseen: HashSet<String> = wanted.iter().filter_map(envelope_identity).collect();
        let files = std::iter::once(self.path.clone()).chain(existing_rotations(&self.path));
        for file in files {
            if unseen.is_empty() {
                break;
            }
            Self::scan(&file, |envelope| {
                if let Some(id) = envelope_identity(&envelope) {
                    unseen.remove(&id);
                }
                !unseen.is_empty()
            })?;
        }
        Ok(wanted
            .iter()
            .filter(|env| envelope_identity(env).is_none_or(|id| unseen.contains(&id)))
            .cloned()
            .collect())
    }

    /// Append `envelopes` in one durable write.
    pub fn append(&self, envelopes: &[TelemetryEnvelope]) -> io::Result<()> {
        if envelopes.is_empty() {
            return Ok(());
        }
        let mut buffer = String::new();
        for env in envelopes {
            buffer.push_str(&serde_json::to_string(env).map_err(io::Error::other)?);
            buffer.push('\n');
        }
        append_durable(&self.path, buffer.as_bytes())
    }

    /// How many runs/jobs/envelopes the journal holds.
    pub fn counts(&self) -> io::Result<JournalCounts> {
        let mut counts = JournalCounts::default();
        self.for_each(|env| {
            counts.envelopes += 1;
            match env.record {
                TelemetryRecord::CiRun(_) => counts.runs += 1,
                TelemetryRecord::CiJob(_) => counts.jobs += 1,
                TelemetryRecord::CiJobLog(_) => counts.job_log_chunks += 1,
                _ => {}
            }
        })?;
        Ok(counts)
    }
}
