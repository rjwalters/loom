//! Point-in-time (knowable-at) filtering for observed records (#10511).
//!
//! The backtest-rigor rule: a record may inform a fit or an estimate at
//! `cutoff` only if Loom had **observed** it by then. Event time is not
//! enough — a CI run that completed at 10:00 but was first polled at 10:20,
//! or a label change backfilled a week later, was not knowable at 10:05, and
//! letting it through makes a backtest read better than serving ever could.
//!
//! So every record a timeline is built from carries an `observed_at` (the
//! stage journal's [`JournalEntry::observed_at`], the CI kinds'
//! `observed_at`), and readers keep exactly the rows with
//! `observed_at <= cutoff` ([`knowable_by`]).
//!
//! # A record with no `observed_at` is not knowable
//!
//! CI records written before #10511 carry none. Their event time is only a
//! *lower* bound on when they became knowable, so falling back to it would
//! re-open the very leak this rule closes. [`knowable_by`] therefore refuses
//! them: a reader that wants old records must supply an honest observation
//! time of its own (e.g. a SigNoz ingest timestamp) rather than this helper
//! guessing one.

use chrono::{DateTime, Utc};

use super::journal::JournalEntry;
use crate::telemetry::ci::{CiDurationRecord, CiJobRecord, CiRunRecord};

/// A record that knows when Loom observed it.
pub trait Observed {
    /// The instant this record became knowable to Loom; `None` when it was
    /// written without one.
    fn observed_at(&self) -> Option<DateTime<Utc>>;
}

/// Whether a record observed at `observed_at` was knowable at `cutoff`:
/// observed **at or before** it. `None` is never knowable (see the module
/// docs).
#[must_use]
pub fn knowable_by(observed_at: Option<DateTime<Utc>>, cutoff: DateTime<Utc>) -> bool {
    observed_at.is_some_and(|at| at <= cutoff)
}

/// The records of `rows` that were knowable at `cutoff`, in their original
/// order.
///
/// [`crate::eta::history::StageSamples::select`] is stricter still
/// (`observed_at < as_of`), so a row this admits at exactly `cutoff` may be
/// refused there, but never the reverse: anything this drops, `select`
/// drops too.
pub fn knowable_at<'a, T, I>(rows: I, cutoff: DateTime<Utc>) -> impl Iterator<Item = &'a T>
where
    T: Observed + 'a,
    I: IntoIterator<Item = &'a T>,
{
    rows.into_iter()
        .filter(move |row| knowable_by(row.observed_at(), cutoff))
}

impl Observed for JournalEntry {
    fn observed_at(&self) -> Option<DateTime<Utc>> {
        Some(self.observed_at)
    }
}

impl Observed for CiRunRecord {
    fn observed_at(&self) -> Option<DateTime<Utc>> {
        self.observed_at
    }
}

impl Observed for CiJobRecord {
    fn observed_at(&self) -> Option<DateTime<Utc>> {
        self.observed_at
    }
}

impl Observed for CiDurationRecord {
    fn observed_at(&self) -> Option<DateTime<Utc>> {
        self.observed_at
    }
}
