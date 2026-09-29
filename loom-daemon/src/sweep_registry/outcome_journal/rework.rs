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

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use serde::Serialize;

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

/// The closed rework-kind vocabulary a writer may mark (the module doc's
/// table). Kept beside `default_classification` so the two cannot drift: a
/// kind here always has a classification there.
pub const REWORK_KINDS: &[&str] = &["rebase", "merge_conflict", "ci_rerun", "rejudge"];

/// Whether `kind` is in the protocol's closed vocabulary. A typo'd kind would
/// otherwise be recorded and then silently read as `environmental` by the
/// catch-all default — plausible, and wrong.
#[must_use]
pub fn known_kind(kind: &str) -> bool {
    REWORK_KINDS.contains(&kind)
}

/// One rework-event marker, exactly as the protocol defines it (module doc):
/// `issue` and `kind` are required, `at` is RFC 3339 and scopes the event to a
/// sweep's window, the rest is optional. The classification is deliberately
/// NOT a writer field here — omit it and [`default_classification`] decides at
/// read time, so the substantive/environmental table stays in one place.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReworkMarker {
    pub at: String,
    pub issue: u32,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_sec: Option<i64>,
}

impl ReworkMarker {
    /// The file line for this marker: one compact JSON object, newline
    /// terminated. The trailing newline is part of the contract — the reader
    /// skips a final partial line, which is the crash window this append-only
    /// shape deliberately accepts.
    pub fn to_line(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string(self).context("serializing rework marker")? + "\n")
    }
}

/// Append one marker to
/// `<workspace_root>/.loom/logs/sweep-rework-events.jsonl` — Issue #9444's
/// first writer (the merge path, via `merge-pr record-rework`). Append-only by
/// protocol: the file is created as needed and never rewritten, and the
/// daemon-side reader never rewrites it either. Errors carry context for the
/// caller's log; the merge path isolates the whole invocation with `|| true`
/// because a lost telemetry marker must never block a merge.
pub fn append_rework_event(
    workspace_root: &std::path::Path,
    marker: &ReworkMarker,
) -> anyhow::Result<()> {
    let path = rework_events_path(workspace_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating rework-marker dir {}", parent.display()))?;
    }
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening rework-marker file {}", path.display()))?;
    file.write_all(marker.to_line()?.as_bytes())
        .with_context(|| format!("appending rework marker to {}", path.display()))?;
    Ok(())
}

/// Resolve the issue a PR belongs to from THIS host's own sweep-outcome
/// telemetry journal: the sweep that opened the PR recorded `repo` (slug) and
/// `pr_number` on its outcome, and the merge runs on the same host, so no
/// forge round trip is needed. The journal's LAST matching record wins
/// (append order is chronological). `None` = this host has no record linking
/// them — a merge run outside a sweep's lifecycle, or a PR opened before the
/// journal existed. Best-effort callers skip the marker rather than guess.
#[must_use]
pub fn issue_for_pr_from_journal(
    workspace_root: &std::path::Path,
    repo_slug: &str,
    pr: u32,
) -> Option<u32> {
    let path = crate::sweep_outcomes::default_outcome_telemetry_path(workspace_root);
    crate::sweep_outcomes::read_all_sweep_outcomes(&path)
        .into_iter()
        .rev()
        .find(|record| record.repo.as_deref() == Some(repo_slug) && record.pr_number == Some(pr))
        .map(|record| record.issue)
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

    // ------------------------------------------------------------------
    // The first writer (Issue #9444): `merge-pr record-rework` path.
    // ------------------------------------------------------------------

    fn marker(issue: u32, kind: &str) -> ReworkMarker {
        ReworkMarker {
            at: "2026-09-30T01:02:03Z".to_string(),
            issue,
            kind: kind.to_string(),
            reason: None,
            duration_sec: None,
        }
    }

    #[test]
    fn first_writer_round_trips_through_the_reader() {
        let dir = TempDir::new().unwrap();
        append_rework_event(
            dir.path(),
            &ReworkMarker {
                reason: Some("base-modified".to_string()),
                ..marker(42, "rebase")
            },
        )
        .unwrap();
        append_rework_event(
            dir.path(),
            &ReworkMarker {
                duration_sec: Some(90),
                ..marker(42, "merge_conflict")
            },
        )
        .unwrap();
        let events = read_rework_events(dir.path(), 42, None);
        assert_eq!(events.len(), 2);
        // The writer deliberately carries no classification; the reader's
        // table decides — one source of truth.
        assert_eq!(events[0].kind, "rebase");
        assert_eq!(events[0].classification.as_deref(), Some("environmental"));
        assert_eq!(events[0].reason.as_deref(), Some("base-modified"));
        assert_eq!(events[1].kind, "merge_conflict");
        assert_eq!(events[1].duration_sec, Some(90));
        // The markers name their issue; another issue's terminal sweep
        // samples none of them.
        assert!(read_rework_events(dir.path(), 43, None).is_empty());
    }

    #[test]
    fn append_creates_missing_dirs_and_is_append_only() {
        let dir = TempDir::new().unwrap();
        // The workspace has no `.loom/logs/` at all yet — the merge path
        // cannot be the one to find that out the hard way.
        append_rework_event(dir.path(), &marker(42, "rebase")).unwrap();
        append_rework_event(dir.path(), &marker(42, "ci_rerun")).unwrap();
        let contents = std::fs::read_to_string(rework_events_path(dir.path())).unwrap();
        assert_eq!(contents.lines().count(), 2, "append, never rewrite: {contents}");
    }

    #[test]
    fn every_known_kind_has_a_default_classification() {
        for kind in REWORK_KINDS {
            let class = default_classification(kind);
            assert!(
                class == "environmental" || class == "substantive",
                "kind {kind} must classify, never fall through to the catch-all silently"
            );
        }
        assert!(!known_kind("rebased"), "a typo'd kind is not in the vocabulary");
    }

    #[test]
    fn issue_for_pr_reads_this_hosts_journal_only() {
        let dir = TempDir::new().unwrap();
        assert_eq!(issue_for_pr_from_journal(dir.path(), "o/r", 7), None);
        // A full record, built the same way lineage.rs's tests build theirs:
        // the journal's serde shape is what it is, no shortcuts.
        let record = |issue: u32, pr: u32| crate::telemetry::SweepOutcomeRecord {
            repo: Some("o/r".to_string()),
            visibility: crate::telemetry::RepoVisibility::Private,
            tokens_status: None,
            tokens_status_reason: None,
            issue,
            sweep_id: format!("s-{issue}-{pr}"),
            model: None,
            effort: None,
            config: Default::default(),
            phase_durations: Vec::new(),
            total_duration_sec: 60,
            result: crate::telemetry::SweepResult::Success,
            pr_number: Some(pr),
            tokens_in: None,
            tokens_out: None,
            lines_added: None,
            lines_deleted: None,
            tokens_by_model: None,
            failure_class: None,
            models_used: None,
            doctor_cycles: None,
            judge_verdicts: None,
            runtime: None,
            provider: None,
            profile: None,
            complexity: None,
            repo_unresolved: false,
            disposition: crate::telemetry::SweepDisposition::Unknown,
            tokens_unattributed: None,
            attempt_index: None,
            previous_sweep_id: None,
            trigger: None,
            rework_events: None,
            pr_numbers: None,
            hw_lines_added: None,
            hw_lines_deleted: None,
            hw_files: None,
            generated_lines: None,
            test_lines: None,
        };
        let envelope = |rec: crate::telemetry::SweepOutcomeRecord| {
            crate::telemetry::TelemetryEnvelope::new(
                "host-a",
                crate::telemetry::TelemetryRecord::SweepOutcome(rec),
            )
        };
        let journal = crate::sweep_outcomes::default_outcome_telemetry_path(dir.path());
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        crate::sweep_outcomes::append_outcome_telemetry(&journal, &envelope(record(42, 7)))
            .unwrap();
        crate::sweep_outcomes::append_outcome_telemetry(&journal, &envelope(record(43, 8)))
            .unwrap();
        assert_eq!(issue_for_pr_from_journal(dir.path(), "o/r", 7), Some(42));
        assert_eq!(issue_for_pr_from_journal(dir.path(), "o/r", 8), Some(43));
        assert_eq!(issue_for_pr_from_journal(dir.path(), "other/r", 7), None);
        assert_eq!(issue_for_pr_from_journal(dir.path(), "o/r", 9), None);
    }
}
