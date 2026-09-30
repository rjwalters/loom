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

use crate::rework_events::KIND_REBASE;
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

/// Every rework event the terminal turn reports for `issue`'s sweep: the
/// marker file's events first (in file order), then the worktree reflog's
/// (oldest first), both scoped to `[started_at, now]`. A missing worktree
/// contributes nothing. The two sources are disjoint by construction (see
/// [`crate::rework_events`]), so this concatenates rather than de-duplicates.
#[must_use]
pub(crate) fn collect_rework_events(
    workspace_root: &std::path::Path,
    worktree: &std::path::Path,
    issue: u32,
    started_at: DateTime<Utc>,
) -> Vec<ReworkEvent> {
    let mut events = read_rework_events(workspace_root, issue, Some(started_at));
    if worktree.exists() {
        events.extend(read_reflog_rework(worktree, Some(started_at)));
    }
    events
}

/// Rework events read off the sweep's own worktree **reflog** — the
/// mechanical writer that needs no role compliance (Issue #9444).
///
/// When a Doctor resolves a merge conflict or a Builder integrates moved
/// main, the worktree's `HEAD` reflog records it durably (`rebase (start)`,
/// `rebase (finish): returning to ...`, `merge origin/main ...`), timestamped.
/// Reading that at the terminal turn means the rework event exists whether or
/// not any prompt remembered to write a marker: the reflog IS the writer.
///
/// Every reflog-derived event is a [`KIND_REBASE`], classified by the shared
/// table in [`crate::rework_events`] (so **environmental** — the ground moved
/// under the work) — a reflog entry cannot distinguish "the judge asked for
/// real changes", which is `rejudge`'s job and arrives via the marker file.
/// This source and the marker file are disjoint by construction: markers
/// record only rework this reflog cannot show (a forge-side branch update, a
/// refusal, a CI rerun, a rejudge), never a local rebase — see
/// [`crate::rework_events`]'s "Two sources" section.
/// Consecutive `rebase (start)`/`rebase (finish)` entries collapse into one
/// event: a rebase is one rework, not two.
///
/// Best-effort on every axis: a missing worktree, a non-git directory, or an
/// unreadable entry skips that entry only.
#[must_use]
pub(crate) fn read_reflog_rework(
    worktree: &std::path::Path,
    window_start: Option<DateTime<Utc>>,
) -> Vec<ReworkEvent> {
    let Ok(output) = std::process::Command::new("git")
        .args(["reflog", "--date=iso", "--no-decorate"])
        .current_dir(worktree)
        .stdin(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut events: Vec<ReworkEvent> = Vec::new();
    let mut in_rebase = false;
    for line in text.lines().rev() {
        // Format: `<sha> HEAD@{<iso date>}: <message>` (--date=iso renders
        // `2026-09-29 12:00:00 +0000` inside the braces).
        let Some((head, message)) = line.split_once(": ") else {
            continue;
        };
        let Some(open) = head.find("HEAD@{") else {
            continue;
        };
        let raw_date = head[open + 6..].trim_end_matches('}').trim();
        let Some(at) = parse_reflog_date(raw_date) else {
            continue;
        };
        if let Some(start) = window_start {
            if at < start {
                continue;
            }
        }
        let lower = message.to_ascii_lowercase();
        if lower.starts_with("rebase (finish)") || lower.starts_with("rebase (abort)") {
            in_rebase = false;
            continue;
        }
        if lower.starts_with("rebase (start)") {
            // Collapse the start/finish pair into one event, anchored at the
            // START (when the rework began).
            in_rebase = true;
            events.push(reflog_event("worktree reflog: rebase".to_string()));
            continue;
        }
        if in_rebase {
            // Inside a rebase pair: the finish arm already handled the close.
            continue;
        }
        if lower.starts_with("merge ") || lower.starts_with("commit (merge)") {
            events.push(reflog_event(format!("worktree reflog: {message}")));
        }
    }
    events
}

/// One reflog-derived event: the shared vocabulary's [`KIND_REBASE`],
/// classified by the shared table rather than a local literal.
fn reflog_event(reason: String) -> ReworkEvent {
    ReworkEvent {
        kind: KIND_REBASE.to_string(),
        reason: Some(reason),
        classification: Some(default_classification(KIND_REBASE).to_string()),
        duration_sec: None,
    }
}

/// Parse a `git reflog --date=iso` timestamp (`2026-09-29 12:00:00 -0700`)
/// into the UTC instant it names. The offset is honoured, not discarded:
/// git renders reflog dates in the host's local zone, so reading the wall
/// clock as if it were UTC shifts every event by the host's offset and drops
/// real rebases out of the window on any non-UTC host (Issue #9553). An
/// offset-less date (never emitted by `--date=iso`, kept for tolerance) is
/// read as UTC.
fn parse_reflog_date(raw: &str) -> Option<DateTime<Utc>> {
    if let Ok(at) = DateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S %z") {
        return Some(at.with_timezone(&Utc));
    }
    chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|at| at.and_utc())
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

    /// A real rebase performed in a real temp repo is observed as one
    /// environmental `rebase` event inside the sweep's window (Issue #9444's
    /// mechanical-writer contract — no marker file involved).
    #[test]
    fn a_real_rebase_is_observed_from_the_worktree_reflog() {
        let git_works = std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success());
        if !git_works {
            return; // no git on PATH: the mechanical observation cannot run
        }
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "base"]);
        run(&["checkout", "-q", "-b", "feature/issue-9"]);
        std::fs::write(repo.join("work.txt"), "work\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "work"]);
        // Main moves after the branch forks.
        run(&["checkout", "-q", "main"]);
        std::fs::write(repo.join("main.txt"), "main\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "main moves"]);
        run(&["checkout", "-q", "feature/issue-9"]);
        // The Doctor's conflict rebase, performed for real.
        run(&["rebase", "main"]);

        let window_start = chrono::Utc::now() - chrono::Duration::hours(1);
        let events = read_reflog_rework(&repo, Some(window_start));
        assert!(
            events.iter().any(|event| event.kind == "rebase"
                && event.classification.as_deref() == Some("environmental")),
            "the rebase must be observed as one environmental event: {events:?}"
        );

        // The terminal turn reads BOTH sources. The merge path's markers
        // (a corroborated conflict refusal, a forge-side base sync) describe
        // rework this worktree's reflog never saw; the local rebase is
        // reported by the reflog alone, exactly once — no source counts
        // another's event.
        let workspace = TempDir::new().unwrap();
        for kind in [
            crate::rework_events::KIND_MERGE_CONFLICT,
            crate::rework_events::KIND_REBASE,
        ] {
            crate::rework_events::append(
                workspace.path(),
                &crate::rework_events::Marker {
                    issue: 9,
                    kind,
                    reason: Some("merge-pr.sh"),
                    classification: None,
                    duration_sec: None,
                },
            )
            .unwrap();
        }
        let all = collect_rework_events(workspace.path(), &repo, 9, window_start);
        let reflog_rebases = all
            .iter()
            .filter(|event| {
                event
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.starts_with("worktree reflog"))
            })
            .count();
        assert_eq!(reflog_rebases, 1, "one local rebase, one event: {all:?}");
        assert_eq!(all.len(), 3, "2 markers + 1 reflog rebase: {all:?}");
        assert_eq!(all[0].kind, "merge_conflict");
        assert_eq!(all[1].reason.as_deref(), Some("merge-pr.sh"));
        assert!(all
            .iter()
            .all(|event| event.classification.as_deref()
                == Some(default_classification(&event.kind))));

        // A worktree that no longer exists contributes nothing, not an error.
        let gone =
            collect_rework_events(workspace.path(), &dir.path().join("reaped"), 9, window_start);
        assert_eq!(gone.len(), 2);
    }

    /// A non-UTC offset names a different instant than the same wall clock
    /// in UTC; the parse must convert, not discard (Issue #9553).
    #[test]
    fn reflog_date_offset_is_converted_to_utc_not_discarded() {
        let expect = |rfc3339: &str| {
            DateTime::parse_from_rfc3339(rfc3339)
                .unwrap()
                .with_timezone(&Utc)
        };
        assert_eq!(
            parse_reflog_date("2026-09-29 10:00:00 -0700"),
            Some(expect("2026-09-29T17:00:00Z"))
        );
        assert_eq!(
            parse_reflog_date("2026-09-29 10:00:00 +0530"),
            Some(expect("2026-09-29T04:30:00Z"))
        );
        assert_eq!(
            parse_reflog_date("2026-09-29 10:00:00 +0000"),
            Some(expect("2026-09-29T10:00:00Z"))
        );
        assert_eq!(parse_reflog_date("2026-09-29 10:00:00"), Some(expect("2026-09-29T10:00:00Z")));
        assert_eq!(parse_reflog_date("not a date"), None);
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
