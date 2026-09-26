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

use super::ledger::{append_durable, read_complete_lines, read_repaired_lines};
use super::records::envelope_identity;

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
#[derive(Debug, Clone)]
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    /// Open the journal **as its writer** (under the cycle lock), repairing
    /// a torn tail left by a crash mid-append.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        read_repaired_lines(&path)?;
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

    /// Every envelope currently in the journal (unparseable lines skipped).
    pub fn read_all(&self) -> io::Result<Vec<TelemetryEnvelope>> {
        Ok(read_complete_lines(&self.path)?
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect())
    }

    /// The identity set of every CI envelope already journaled — the
    /// "already emitted" test a pending-unit replay uses.
    pub fn identities(&self) -> io::Result<HashSet<String>> {
        Ok(self
            .read_all()?
            .iter()
            .filter_map(envelope_identity)
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
        for env in self.read_all()? {
            counts.envelopes += 1;
            match env.record {
                TelemetryRecord::CiRun(_) => counts.runs += 1,
                TelemetryRecord::CiJob(_) => counts.jobs += 1,
                TelemetryRecord::CiJobLog(_) => counts.job_log_chunks += 1,
                _ => {}
            }
        }
        Ok(counts)
    }
}
