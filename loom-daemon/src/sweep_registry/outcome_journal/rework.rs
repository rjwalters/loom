//! The in-sweep **rework event** marker protocol (Issue #9444).
//!
//! Only the code path that *performs* a rework knows it happened — today the
//! merge path's stale-base handling (`merge-pr.sh`'s rebase-before-merge),
//! tomorrow the doctor-claim and CI-fix paths. The daemon cannot observe
//! those from outside, so the protocol is a tiny append-only marker file the
//! performing path writes and the terminal outcome samples:
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
//! issue + window, and the classification table below fills the default when
//! the writer did not classify. Lines that fail to parse are skipped, never
//! fatal — a half-written line (the writer crashed mid-append) must not take
//! the outcome journal down.
//!
//! The vocabulary and the substantive/environmental table are normative in
//! `telemetry-schema.md`.

use chrono::{DateTime, Utc};

use crate::telemetry::ReworkEvent;

/// Where the markers live, under the workspace root.
pub(crate) const REWORK_EVENTS_FILENAME: &str = "sweep-rework-events.jsonl";

/// The default substantive/environmental classification per rework kind
/// (Issue #9444's table): a judge asking for real changes or a re-judge is
/// the work being hard; the ground moving (main advanced, a conflict, CI
/// flake) is the environment.
#[must_use]
pub(crate) fn default_classification(kind: &str) -> &'static str {
    match kind {
        "rejudge" => "substantive",
        "rebase" | "merge_conflict" | "ci_rerun" => "environmental",
        _ => "environmental",
    }
}

/// Path of the marker file for `workspace_root`.
#[must_use]
pub(crate) fn rework_events_path(workspace_root: &std::path::Path) -> std::path::PathBuf {
    workspace_root
        .join(".loom")
        .join("logs")
        .join(REWORK_EVENTS_FILENAME)
}

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
}
