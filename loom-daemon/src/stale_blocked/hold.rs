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

/// The own-line permanent-block marker (#8742, #13889).
pub const PERMANENT_BLOCK_MARKER: &str = "<!-- loom:permanent-block -->";

/// Own-line prefixes that make a trusted comment a *hold* comment (#13889): a
/// park record, an operator decision, a Champion/operator hold-restore note.
const HOLD_MARKERS: &[&str] = &["loom:park ", "loom:operator-decision", "loom:hold-restored"];

fn own_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(|l| l.trim_matches(|c| c == ' ' || c == '\t' || c == '\r'))
}

/// Whether `text` carries the permanent-block marker alone on a line — the
/// same own-line rule as `guide.md`'s `has_permanent_block()`, so a marker
/// quoted mid-sentence does not count.
#[must_use]
pub fn has_permanent_block(text: &str) -> bool {
    own_lines(text).any(|l| l == PERMANENT_BLOCK_MARKER)
}

/// Which hold marker opens a line of `text`, if any (the marker name, without
/// the `<!-- ` opener).
#[must_use]
pub fn hold_marker(text: &str) -> Option<&'static str> {
    own_lines(text).find_map(|l| {
        let rest = l.strip_prefix("<!-- ")?;
        HOLD_MARKERS
            .iter()
            .find(|m| rest.starts_with(**m))
            .map(|m| m.trim_end())
    })
}

/// The newest trusted hold comment among `comments` (listing order breaks
/// ties and orders undated ones).
#[must_use]
pub fn newest_hold(
    comments: &[crate::dep_recheck::extract::Comment],
) -> Option<super::HoldComment> {
    comments
        .iter()
        .enumerate()
        .filter(|(_, c)| c.trusted)
        .filter_map(|(i, c)| Some((i, c, hold_marker(&c.body)?)))
        .max_by_key(|(i, c, _)| (when(c.created_at.as_deref()), *i))
        .map(|(_, c, marker)| super::HoldComment {
            at: c.created_at.clone(),
            marker: marker.to_string(),
        })
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

#[cfg(test)]
mod marker_tests {
    use super::*;
    use crate::dep_recheck::extract::Comment;

    fn c(body: &str, at: &str, trusted: bool) -> Comment {
        Comment {
            body: body.to_string(),
            created_at: Some(at.to_string()),
            trusted,
            ..Comment::default()
        }
    }

    #[test]
    fn permanent_block_marker_must_stand_on_its_own_line() {
        assert!(has_permanent_block("note\n  <!-- loom:permanent-block -->\t\r\nmore"));
        assert!(has_permanent_block("<!-- loom:permanent-block -->"));
        assert!(!has_permanent_block("see the `<!-- loom:permanent-block -->` marker"));
        assert!(!has_permanent_block("<!-- loom:permanent-block --> trailing"));
    }

    #[test]
    fn hold_marker_names_the_opening_marker() {
        assert_eq!(
            hold_marker("x\n<!-- loom:operator-decision v=1 -->"),
            Some("loom:operator-decision")
        );
        assert_eq!(hold_marker("<!-- loom:hold-restored -->"), Some("loom:hold-restored"));
        assert_eq!(hold_marker("<!-- loom:park Blocked by: #1 -->"), Some("loom:park"));
        assert_eq!(hold_marker("quoting <!-- loom:hold-restored --> inline"), None);
        assert_eq!(hold_marker("just a comment"), None);
    }

    #[test]
    fn newest_hold_ignores_untrusted_and_plain_comments() {
        let comments = vec![
            c("<!-- loom:hold-restored -->", "2026-11-01T00:00:00Z", true),
            c("<!-- loom:operator-decision -->", "2026-11-09T00:00:00Z", false),
            c("hello", "2026-11-10T00:00:00Z", true),
        ];
        let h = newest_hold(&comments).unwrap();
        assert_eq!(h.marker, "loom:hold-restored");
        assert_eq!(h.at.as_deref(), Some("2026-11-01T00:00:00Z"));
        assert!(newest_hold(&comments[1..]).is_none());
    }
}
