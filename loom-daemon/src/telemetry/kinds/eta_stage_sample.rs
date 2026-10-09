//! `eta.stage_sample` (#10756): one row of the ETA stage journal
//! (`eta::journal`, `.loom/logs/eta-stage-samples.jsonl`), exported verbatim.
//!
//! # Why this kind exists
//!
//! The stage journal is the only record of what the tracker saw: each PR's
//! whole label set at every label transition (`label.transition`), its labels
//! at first sight (`label.first_seen`), the `merge_hold` overlay, and the
//! sweep boundaries a host's own bus saw. It stayed on the host that wrote it,
//! so no SigNoz query could rebuild a label or hold timeline after the fact,
//! and the SigNoz timeline reader's daemon label-set path
//! (`eta::fleet_signoz_timeline_rows`) had no live producer.
//! `eta.stage_outcome` (#10929) covers only rows that close a stage, and only
//! on the ETA authority; this kind covers every row.
//!
//! # Every host
//!
//! A row is offered when it is appended, on **every** host, the ETA authority
//! or not (`observability::eta::append_journal`). A non-authority host
//! journals its own sweeps' bus events (`authority::journal_only`), and those
//! now reach SigNoz too.
//!
//! # Times
//!
//! - `observed_at` is when the tracker observed the row: for a listing diff,
//!   the poll, late by at most `resolution_sec`.
//! - `forge_at` is the forge's own instant, when the daemon knows it: a
//!   merge's `merged_at`, or the label application the label timeline dated a
//!   first sighting from. A listing diff knows only the poll, so it has none.
//!
//! The log record's time is `forge_at`, else `observed_at`; its observed
//! timestamp is the export (knowable-at). The body is the row's JSON.
//!
//! **OTLP only.** **Provenance is required**: a row whose build provenance
//! does not validate is never exported.

use chrono::{DateTime, Utc};

pub use crate::eta::journal::JournalEntry as EtaStageSampleRecord;

impl EtaStageSampleRecord {
    /// Whether the row carries valid provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.loom.is_valid()
    }

    /// The log record's time: the forge instant when known, else the
    /// observation, never later than the observation.
    #[must_use]
    pub fn record_time(&self) -> DateTime<Utc> {
        self.forge_at
            .map_or(self.observed_at, |forge| forge.min(self.observed_at))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "eta_stage_sample_tests.rs"]
mod tests;
