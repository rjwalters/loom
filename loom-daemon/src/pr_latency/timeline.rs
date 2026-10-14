//! Parse a REST `issues/<n>/timeline` response into [`PrEvent`]s.
//!
//! Moved here from `cli/pr_latency_cmd.rs` (#10263) so every reader of
//! a PR's history uses one parser — a second copy would let two
//! drift on exactly the event vocabulary every stage boundary is read from.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::PrEvent;

/// One `issues/<n>/timeline` entry, in the shapes the derivation reads.
///
/// `committed` entries carry no `created_at` — their time is the commit
/// object's committer date. See [`PrEvent::Pushed`] for why that is an
/// acceptable stand-in for a push time here and where it is not.
#[derive(Debug, Clone, Deserialize)]
struct TimelineEntry {
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    label: Option<LabelRef>,
    #[serde(default)]
    committer: Option<GitIdent>,
    #[serde(default)]
    author: Option<GitIdent>,
}

#[derive(Debug, Clone, Deserialize)]
struct LabelRef {
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GitIdent {
    #[serde(default)]
    date: Option<DateTime<Utc>>,
}

/// Parse `gh api --paginate` output into events, or `None` if unreadable.
///
/// Handles both shapes `gh` produces for a paginated array endpoint: one merged
/// top-level array, and several concatenated arrays. Accepting only the first
/// would silently return an empty log for any PR with more than 100 timeline
/// entries — exactly the long-lived PRs this exists to explain.
#[must_use]
pub fn parse_timeline(bytes: &[u8]) -> Option<Vec<PrEvent>> {
    parse_timeline_page(bytes).map(|(events, _)| events)
}

/// [`parse_timeline`], plus how many raw timeline entries the input held —
/// what an explicitly paged reader needs to recognise a short (last) page,
/// since the event list drops every entry kind no segment reads.
#[must_use]
pub fn parse_timeline_page(bytes: &[u8]) -> Option<(Vec<PrEvent>, usize)> {
    let mut entries: Vec<TimelineEntry> = Vec::new();
    let mut arrays = 0usize;
    let mut stream = serde_json::Deserializer::from_slice(bytes).into_iter::<Vec<TimelineEntry>>();
    for chunk in stream.by_ref() {
        entries.extend(chunk.ok()?);
        arrays += 1;
    }
    // Non-empty output that yielded **no array at all** is a shape this does not
    // understand (an error object, a truncated body); report it as incomplete
    // rather than as "no events", so the PR is excluded from the distributions
    // instead of being recorded as a fast one.
    //
    // The test is "how many arrays parsed", not "are there entries": a PR whose
    // timeline is genuinely empty yields `[]` — possibly several of them across
    // `--paginate` pages, which a literal `trimmed != "[]"` check would have
    // misread as unreadable.
    if arrays == 0 && !bytes.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    let raw = entries.len();
    Some((entries.iter().filter_map(to_event).collect(), raw))
}

fn to_event(e: &TimelineEntry) -> Option<PrEvent> {
    let kind = e.event.as_deref()?;
    match kind {
        "labeled" => Some(PrEvent::Labeled {
            label: e.label.as_ref()?.name.clone(),
            at: e.created_at?,
        }),
        "unlabeled" => Some(PrEvent::Unlabeled {
            label: e.label.as_ref()?.name.clone(),
            at: e.created_at?,
        }),
        "committed" => {
            // Committer date first: for a rebase it is the rewrite time, which
            // is the push. Author date is the fallback for the rare entry with
            // no committer block.
            let at = e
                .committer
                .as_ref()
                .and_then(|c| c.date)
                .or_else(|| e.author.as_ref().and_then(|a| a.date))?;
            Some(PrEvent::Pushed { at })
        }
        "head_ref_force_pushed" => Some(PrEvent::Pushed { at: e.created_at? }),
        "merged" => Some(PrEvent::Merged { at: e.created_at? }),
        "closed" => Some(PrEvent::Closed { at: e.created_at? }),
        "reopened" => Some(PrEvent::Reopened { at: e.created_at? }),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_single_merged_array() {
        let json = br#"[
            {"event":"labeled","created_at":"2026-09-01T00:00:00Z","label":{"name":"loom:pr"}},
            {"event":"unlabeled","created_at":"2026-09-01T01:00:00Z","label":{"name":"loom:pr"}},
            {"event":"merged","created_at":"2026-09-01T02:00:00Z"}
        ]"#;
        let events = parse_timeline(json).unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], PrEvent::Labeled { .. }));
        assert!(matches!(events[1], PrEvent::Unlabeled { .. }));
        assert!(matches!(events[2], PrEvent::Merged { .. }));
    }

    #[test]
    fn parses_concatenated_pages() {
        // What `gh api --paginate` emits when it does not merge the arrays.
        let json = br#"[{"event":"merged","created_at":"2026-09-01T02:00:00Z"}]
                       [{"event":"labeled","created_at":"2026-09-01T00:00:00Z","label":{"name":"loom:pr"}}]"#;
        let events = parse_timeline(json).unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn a_commit_entry_becomes_a_push_from_its_committer_date() {
        let json = br#"[{"event":"committed","sha":"abc",
            "author":{"date":"2026-08-01T00:00:00Z"},
            "committer":{"date":"2026-09-01T00:00:00Z"}}]"#;
        let events = parse_timeline(json).unwrap();
        assert_eq!(events.len(), 1);
        let PrEvent::Pushed { at } = events[0] else {
            panic!("expected a push, got {:?}", events[0]);
        };
        assert_eq!(at.to_rfc3339(), "2026-09-01T00:00:00+00:00");
    }

    #[test]
    fn a_force_push_is_a_push() {
        let json = br#"[{"event":"head_ref_force_pushed","created_at":"2026-09-01T00:00:00Z"}]"#;
        assert!(matches!(parse_timeline(json).unwrap()[0], PrEvent::Pushed { .. }));
    }

    #[test]
    fn unrecognised_and_malformed_entries_are_dropped_not_fatal() {
        let json = br#"[
            {"event":"subscribed","created_at":"2026-09-01T00:00:00Z"},
            {"event":"labeled","created_at":"2026-09-01T00:00:00Z"},
            {"event":"labeled","label":{"name":"loom:pr"}},
            {"event":"merged","created_at":"2026-09-01T02:00:00Z"}
        ]"#;
        // A labeled event missing its label, and one missing its timestamp,
        // are both unusable; the merge still is.
        let events = parse_timeline(json).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], PrEvent::Merged { .. }));
        // …but every raw entry still counts toward the page length.
        assert_eq!(parse_timeline_page(json).unwrap().1, 4);
    }

    #[test]
    fn close_and_reopen_entries_are_kept_with_their_times() {
        let json = br#"[
            {"event":"closed","created_at":"2026-09-01T00:00:00Z"},
            {"event":"reopened","created_at":"2026-09-01T01:00:00Z"},
            {"event":"closed","created_at":"2026-09-01T02:00:00Z"}
        ]"#;
        let events = parse_timeline(json).unwrap();
        assert!(matches!(events[0], PrEvent::Closed { .. }));
        assert!(matches!(events[1], PrEvent::Reopened { .. }));
        assert!(matches!(events[2], PrEvent::Closed { .. }));
    }

    #[test]
    fn empty_and_blank_outputs_are_empty_not_unreadable() {
        assert_eq!(parse_timeline(b"[]").unwrap().len(), 0);
        assert_eq!(parse_timeline(b"  \n").unwrap().len(), 0);
    }

    #[test]
    fn several_empty_pages_are_still_a_complete_empty_timeline() {
        // `gh api --paginate` can emit one `[]` per page. Judging readability by
        // the entry count rather than the number of arrays parsed would call
        // this unreadable and drop a legitimately empty PR from the sample.
        assert_eq!(parse_timeline(b"[]\n[]\n").unwrap().len(), 0);
    }

    #[test]
    fn garbage_is_unreadable_rather_than_an_empty_timeline() {
        assert!(parse_timeline(b"not json at all").is_none());
        assert!(parse_timeline(b"{\"message\":\"Not Found\"}").is_none());
    }
}
