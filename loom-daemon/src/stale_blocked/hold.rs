//! Which body park record documents the **current** hold (#10558).
//!
//! Park records are append-only: a release flips labels and leaves the record
//! in the body. So "the body has a reason-only record" does not mean the
//! current `loom:blocked` is documented — an issue once held with a reason and
//! later re-blocked bare still carries the old record. [`latest`] picks the
//! current one: of several records, the one with the newest `at=` (undated
//! records sort oldest; ties keep body order).

use chrono::{DateTime, FixedOffset};

use crate::park_record::ParkRecord;

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
}
