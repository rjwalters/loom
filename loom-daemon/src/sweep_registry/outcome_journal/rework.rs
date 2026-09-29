//! The **reader** half of the in-sweep rework-event marker protocol (Issue
//! #9444).
//!
//! Only the code path that *performs* a rework knows it happened — the merge
//! path's stale-base sync and conflict refusal (`merge-pr.sh`, via
//! `loom-daemon record-rework`), and the doctor-claim / CI-fix paths after it.
//! The daemon cannot observe those from outside, so the protocol is a tiny
//! append-only marker file the performing path writes and the terminal outcome
//! samples:
//!
//! ```text
//! <workspace_root>/.loom/logs/sweep-rework-events.jsonl
//! {"at":"2026-09-30T01:02:03Z","issue":42,"kind":"rebase","reason":"main moved",
//!  "classification":"environmental","duration_sec":18}
//! ```
//!
//! One JSON object per line. `issue` and `kind` are required; `at` (RFC 3339)
//! is what scopes an event to a sweep's window — an event without `at` is
//! delivered to the issue's next terminal sweep, whatever it is. The
//! daemon-side reader here never rewrites the file: events are filtered by
//! issue + window, and [`crate::rework_events::default_classification`] fills
//! the default when the writer did not classify. Lines that fail to parse are
//! skipped, never fatal — a half-written line (the writer crashed mid-append)
//! must not take the outcome journal down. A *foreign* writer's kind is
//! accepted and classified by the table's catch-all; only
//! [`crate::rework_events::append`] (this repo's own writer) refuses one.
//!
//! The shared half of the protocol — the path, the `kind` vocabulary, the
//! classification table, and the writer — lives in [`crate::rework_events`],
//! so the two ends cannot drift. The vocabulary and the
//! substantive/environmental table are normative in `telemetry-schema.md`.

use chrono::{DateTime, Utc};

pub(crate) use crate::rework_events::{default_classification, path as rework_events_path};
use crate::telemetry::ReworkEvent;

/// Read this issue's rework events inside `[window_start, now]`. Best-effort
/// and order-preserving: a missing file, an unreadable line, or a bad
/// timestamp skips that line only. `window_start == None` disables the time
/// filter (used when the sweep's own start is unknown).
#[must_use]
pub(crate) fn read_rework_events(
    workspace_root: &std::path::Path,
    issue: u32,
    window_start: Option<DateTime<Utc>>,
) -> Vec<ReworkEvent> {
    let Ok(contents) = std::fs::read_to_string(rework_events_path(workspace_root)) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            // Required fields; a foreign writer's line for another issue (or
            // a malformed one) is skipped, not fatal.
            (value.get("issue")?.as_u64()? == u64::from(issue)).then_some(())?;
            let kind = value.get("kind")?.as_str()?.to_string();
            let at = value
                .get("at")
                .and_then(|at| at.as_str())
                .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                .map(|at| at.with_timezone(&Utc));
            if let (Some(start), Some(at)) = (window_start, at) {
                if at < start {
                    return None;
                }
            }
            let classification = value
                .get("classification")
                .and_then(|c| c.as_str())
                .map(ToString::to_string)
                .unwrap_or_else(|| default_classification(&kind).to_string());
            Some(ReworkEvent {
                kind,
                reason: value
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .map(ToString::to_string),
                classification: Some(classification),
                duration_sec: value.get("duration_sec").and_then(|d| d.as_i64()),
            })
        })
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_markers(dir: &TempDir, lines: &[String]) {
        let path = rework_events_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, lines.join("\n")).unwrap();
    }

    #[test]
    fn reads_matching_events_and_defaults_classification() {
        let dir = TempDir::new().unwrap();
        write_markers(
            &dir,
            &[
                serde_json::json!({"at":"2026-09-30T01:00:00Z","issue":42,"kind":"rebase","reason":"main moved"})
                    .to_string(),
                serde_json::json!({"at":"2026-09-30T01:05:00Z","issue":43,"kind":"rebase"}).to_string(),
                serde_json::json!({"at":"2026-09-30T01:07:00Z","issue":42,"kind":"rejudge","classification":"substantive"}).to_string(),
            ],
        );
        let events = read_rework_events(dir.path(), 42, None);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "rebase");
        assert_eq!(events[0].classification.as_deref(), Some("environmental"));
        assert_eq!(events[0].reason.as_deref(), Some("main moved"));
        assert_eq!(events[1].kind, "rejudge");
        assert_eq!(events[1].classification.as_deref(), Some("substantive"));
    }

    #[test]
    fn window_filters_and_garbage_lines_are_skipped() {
        let dir = TempDir::new().unwrap();
        write_markers(
            &dir,
            &[
                serde_json::json!({"at":"2026-09-29T00:00:00Z","issue":42,"kind":"rebase"}).to_string(),
                "not json at all".to_string(),
                serde_json::json!({"at":"2026-09-30T02:00:00Z","issue":42,"kind":"merge_conflict","duration_sec":90}).to_string(),
            ],
        );
        let start = DateTime::parse_from_rfc3339("2026-09-30T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let events = read_rework_events(dir.path(), 42, Some(start));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "merge_conflict");
        assert_eq!(events[0].duration_sec, Some(90));
    }

    #[test]
    fn missing_file_is_empty_not_fatal() {
        let dir = TempDir::new().unwrap();
        assert!(read_rework_events(dir.path(), 42, None).is_empty());
    }

    #[test]
    fn classification_table() {
        assert_eq!(default_classification("rejudge"), "substantive");
        assert_eq!(default_classification("rebase"), "environmental");
        assert_eq!(default_classification("merge_conflict"), "environmental");
        assert_eq!(default_classification("ci_rerun"), "environmental");
    }

    /// The protocol closes: what [`crate::rework_events::append`] writes is
    /// what this reader returns, for **every** kind in the vocabulary.
    ///
    /// Before #9444's writer slice, `read_rework_events` was the only
    /// participant in this protocol — nothing wrote the file, so
    /// `rework_events` could only ever be absent and every rollup over it
    /// reported a fleet with no rework. Nothing failed; the number was simply
    /// always zero. This test is the thing that stays true once both halves
    /// exist: it drives the real writer rather than a restatement of its
    /// output format, so a field the writer renames fails here instead of
    /// silently reading back as an unclassified event.
    #[test]
    fn what_the_writer_writes_is_what_the_reader_reads() {
        use crate::rework_events::{append, Marker, KINDS};

        let dir = TempDir::new().unwrap();
        let before = Utc::now();
        for kind in KINDS {
            append(
                dir.path(),
                &Marker {
                    issue: 9444,
                    kind,
                    reason: Some("main moved"),
                    classification: None,
                    duration_sec: Some(7),
                },
            )
            .unwrap();
        }
        // A sibling issue's markers share the file and must not leak in.
        append(
            dir.path(),
            &Marker {
                issue: 9445,
                kind: "rebase",
                reason: None,
                classification: None,
                duration_sec: None,
            },
        )
        .unwrap();

        let events = read_rework_events(dir.path(), 9444, Some(before));
        assert_eq!(events.len(), KINDS.len(), "{events:?}");
        for (event, kind) in events.iter().zip(KINDS) {
            assert_eq!(&event.kind, kind);
            assert_eq!(event.reason.as_deref(), Some("main moved"));
            assert_eq!(event.duration_sec, Some(7));
            assert_eq!(
                event.classification.as_deref(),
                Some(default_classification(kind)),
                "the writer stamped a classification this reader does not agree with"
            );
        }

        // …and the same markers are outside a window that starts after them.
        let after = Utc::now() + chrono::Duration::seconds(1);
        assert!(read_rework_events(dir.path(), 9444, Some(after)).is_empty());
    }
}
