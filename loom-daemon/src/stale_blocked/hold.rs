//! Which body park record documents the **current** hold (#10558).
//!
//! Park records are append-only: a release flips labels and leaves the record
//! in the body. So "the body has a reason-only record" does not mean the
//! current `loom:blocked` is documented — an issue once held with a reason and
//! later re-blocked bare still carries the old record. Two rules:
//!
//! - [`latest`]: of several records, the one with the newest `at=` (undated
//!   records sort oldest; ties keep body order) is the current one.
//! - [`documents_current_block`]: a dated record documents the block only if it
//!   is not older than the latest `loom:blocked` application, less
//!   [`WRITE_SLACK_SECS`] — every writer writes the record *before* the label
//!   (`park-record apply`, `park_hold.rs`), so a current record is a few
//!   seconds older than its label.

use chrono::{DateTime, Duration, FixedOffset};

use crate::park_record::ParkRecord;

/// Allowed gap between a record's `at=` and the label write that follows it.
pub const WRITE_SLACK_SECS: i64 = 600;

fn when(at: Option<&str>) -> Option<DateTime<FixedOffset>> {
    at.and_then(|a| DateTime::parse_from_rfc3339(a.trim()).ok())
}

/// The newest record by `at=`; undated records sort oldest, ties keep the
/// later body position.
#[must_use]
pub fn latest(records: &[ParkRecord]) -> Option<&ParkRecord> {
    records
        .iter()
        .enumerate()
        .max_by_key(|(i, r)| (when(r.at.as_deref()), *i))
        .map(|(_, r)| r)
}

/// The newest reason-only record (no blocker, non-empty reason) in `body`.
#[must_use]
pub fn latest_reasoned(body: &str) -> Option<ParkRecord> {
    let records: Vec<ParkRecord> = crate::park_record::parse(body)
        .into_iter()
        .filter(|r| {
            r.blocker.is_none() && r.reason.as_deref().is_some_and(|x| !x.trim().is_empty())
        })
        .collect();
    latest(&records).cloned()
}

/// Whether a record written at `record_at` documents a `loom:blocked` last
/// applied at `labeled_at`. Fails toward "documented" (no write) when either
/// time is missing or unreadable: an undatable record cannot be shown stale.
#[must_use]
pub fn documents_current_block(record_at: Option<&str>, labeled_at: Option<&str>) -> bool {
    match (when(record_at), when(labeled_at)) {
        (Some(r), Some(l)) => r + Duration::seconds(WRITE_SLACK_SECS) >= l,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::park_record::render_park;

    #[test]
    fn latest_prefers_the_newest_dated_record_over_body_order() {
        let body = format!(
            "{}\n{}\n{}",
            render_park(&[], Some("a"), Some("2026-10-06T12:00:00Z"), Some("new")),
            render_park(&[], Some("b"), Some("2026-01-01T00:00:00Z"), Some("old")),
            render_park(&[], Some("c"), None, Some("undated")),
        );
        assert_eq!(latest_reasoned(&body).unwrap().reason.as_deref(), Some("new"));
    }

    #[test]
    fn a_record_older_than_the_label_does_not_document_it() {
        let rec = Some("2026-01-01T00:00:00Z");
        assert!(!documents_current_block(rec, Some("2026-10-06T00:00:00Z")));
        // Record written seconds before its own label write: current.
        assert!(documents_current_block(
            Some("2026-10-06T00:00:00Z"),
            Some("2026-10-06T00:00:30Z")
        ));
        // Undatable: fail toward documented.
        assert!(documents_current_block(None, Some("2026-10-06T00:00:00Z")));
        assert!(documents_current_block(rec, None));
    }
}
