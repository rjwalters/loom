//! The **rework-event marker protocol** (Issue #9444) — the one place both
//! ends of it are defined.
//!
//! Only the code path that *performs* a rework knows it happened. The merge
//! path syncs a branch whose base moved; it refuses a PR that genuinely
//! conflicts; a doctor claimed for `loom:merge-conflict` resolves one. None of
//! that is observable from outside the process that did it, so the daemon
//! cannot derive `rework_events` — something has to *tell* it.
//!
//! The protocol is a tiny append-only JSONL marker file under the workspace
//! root:
//!
//! ```text
//! <workspace_root>/.loom/logs/sweep-rework-events.jsonl
//! {"at":"2026-09-30T01:02:03Z","issue":42,"kind":"rebase","reason":"main moved",
//!  "classification":"environmental","duration_sec":18}
//! ```
//!
//! This module owns the *shared* half — where the file lives, what `kind`
//! values exist, how an unclassified kind is classified, and [`append`], the
//! writer. The reader lives beside the outcome journal that samples it
//! (`sweep_registry::outcome_journal::rework`) and delegates here for all four,
//! so a writer and a reader cannot disagree about the protocol they share.
//!
//! The vocabulary and the substantive/environmental table are normative in
//! `telemetry-schema.md`.
//!
//! # Writers
//!
//! `loom-daemon record-rework` (`cli::record_rework`) is the entry point, and
//! `merge-pr.sh` is its first caller — the shell-language policy points new
//! executable logic at a subcommand rather than at a script already frozen by
//! the file-size ratchet, and a subcommand is also what makes the marker
//! writable from a doctor/CI path later without a third copy of this format.
//!
//! # Two sources, one vocabulary — and what a writer must NOT mark
//!
//! `rework_events` has a second, writer-free source: the sweep worktree's
//! own `HEAD` reflog, read at the terminal turn
//! (`outcome_journal::rework::read_reflog_rework`, #9511). A rebase or a
//! merge of moved main performed *in the worktree* records itself there, and
//! is reported as a [`KIND_REBASE`] event classified by this module's table.
//! Both sources are read, and they are kept **disjoint by construction**, not
//! by a fuzzy de-duplication after the fact:
//!
//! - a marker records only rework the worktree reflog *cannot* show — a
//!   forge-side branch update (`merge-pr.sh`'s `forge_update_branch`, which
//!   never touches the local worktree), a merge refusal, a CI rerun, a
//!   rejudge;
//! - a rebase or merge performed with local git in the sweep's worktree is
//!   **never** marked: the reflog already reports it, and a marker would
//!   count it twice.
//!
//! A future writer (a Doctor or CI-fix path) must keep to that rule.
//!
//! # Durability and failure
//!
//! A marker is telemetry: **it may never fail the operation it describes.**
//! [`append`] returns an error rather than panicking, and every caller is
//! expected to discard it. The file is opened `O_APPEND` and written with a
//! single `write` of one line; on Linux that is atomic for a regular file
//! under `PIPE_BUF`, which is why [`MAX_REASON_CHARS`] bounds the one
//! caller-supplied free-text field. A torn line is not fatal either way — the
//! reader skips a line it cannot parse.

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Utc;

/// The marker file's name, under `<workspace_root>/.loom/logs/`.
pub const FILENAME: &str = "sweep-rework-events.jsonl";

/// Every `kind` the vocabulary defines (Issue #9444). Writers are validated
/// against this list: a typo that widens the vocabulary is a silent
/// cardinality leak into every downstream rollup, so it is refused at the
/// writer instead of being classified by [`default_classification`]'s
/// catch-all.
pub const KINDS: &[&str] = &[
    KIND_REBASE,
    KIND_MERGE_CONFLICT,
    KIND_CI_RERUN,
    KIND_REJUDGE,
];

/// `kind` for the ground moving under the work: main advanced and the branch
/// was synced. The only kind the worktree-reflog source emits.
pub const KIND_REBASE: &str = "rebase";
/// `kind` for a branch that genuinely conflicts with its base.
pub const KIND_MERGE_CONFLICT: &str = "merge_conflict";
/// `kind` for a CI rerun.
pub const KIND_CI_RERUN: &str = "ci_rerun";
/// `kind` for a judge asking for real changes.
pub const KIND_REJUDGE: &str = "rejudge";

/// The two classifications, and the only values a rollup buckets on.
pub const CLASSIFICATIONS: &[&str] = &["substantive", "environmental"];

/// Free-text `reason` is truncated to this many characters. Keeps one marker
/// line inside the single-`write` atomicity window described in the module
/// docs, and keeps a forge error message from turning the marker file into a
/// log.
pub const MAX_REASON_CHARS: usize = 200;

/// Above this size the marker file is pruned to its most recent
/// [`KEEP_LINES`] lines after an append.
///
/// The file has no other reaper: the reader filters by issue and window and
/// deliberately never rewrites it (a terminal sweep must not be able to lose
/// a sibling sweep's markers). Left alone it would grow without bound and be
/// re-read in full on every terminal transition.
pub const MAX_MARKER_BYTES: u64 = 128 * 1024;

/// How many trailing lines a prune keeps.
pub const KEEP_LINES: usize = 512;

/// The default substantive/environmental classification per rework kind
/// (Issue #9444's table): a judge asking for real changes or a re-judge is the
/// work being hard; the ground moving (main advanced, a conflict, CI flake) is
/// the environment.
#[must_use]
pub fn default_classification(kind: &str) -> &'static str {
    match kind {
        KIND_REJUDGE => "substantive",
        KIND_REBASE | KIND_MERGE_CONFLICT | KIND_CI_RERUN => "environmental",
        // Unreachable through `append` (which validates against `KINDS`), but
        // the reader accepts a foreign writer's line and must classify it.
        // "The ground moved" is the conservative default: it does not inflate
        // the substantive bucket, which is the one read as "this issue is
        // hard".
        _ => "environmental",
    }
}

/// Path of the marker file for `workspace_root`.
#[must_use]
pub fn path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".loom").join("logs").join(FILENAME)
}

/// One marker to append.
#[derive(Debug, Clone)]
pub struct Marker<'a> {
    /// The issue whose sweep this rework belongs to.
    pub issue: u32,
    /// One of [`KINDS`].
    pub kind: &'a str,
    /// Why it happened, when the caller knows. Truncated to
    /// [`MAX_REASON_CHARS`].
    pub reason: Option<&'a str>,
    /// Overrides [`default_classification`]. Must be one of
    /// [`CLASSIFICATIONS`].
    pub classification: Option<&'a str>,
    /// How long the rework took, when the caller measured it. Absent is
    /// "unknown", never zero — a rollup counts it as an *open* event rather
    /// than smoothing it to a fabricated figure.
    pub duration_sec: Option<i64>,
}

/// Append one marker under `workspace_root`, returning the file written.
///
/// # Errors
///
/// An unknown `kind` or `classification` (a vocabulary error, the caller's
/// bug) and any I/O failure. Callers are telemetry callers: discard it.
pub fn append(workspace_root: &Path, marker: &Marker<'_>) -> std::io::Result<PathBuf> {
    if !KINDS.contains(&marker.kind) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unknown rework kind `{}`; expected one of {KINDS:?}", marker.kind),
        ));
    }
    if let Some(class) = marker.classification {
        if !CLASSIFICATIONS.contains(&class) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("unknown classification `{class}`; expected one of {CLASSIFICATIONS:?}"),
            ));
        }
    }

    let mut line = serde_json::Map::new();
    line.insert("at".into(), Utc::now().to_rfc3339().into());
    line.insert("issue".into(), marker.issue.into());
    line.insert("kind".into(), marker.kind.into());
    if let Some(reason) = marker.reason.map(str::trim).filter(|r| !r.is_empty()) {
        let reason: String = reason.chars().take(MAX_REASON_CHARS).collect();
        line.insert("reason".into(), reason.into());
    }
    line.insert(
        "classification".into(),
        marker
            .classification
            .unwrap_or_else(|| default_classification(marker.kind))
            .into(),
    );
    if let Some(duration_sec) = marker.duration_sec {
        line.insert("duration_sec".into(), duration_sec.into());
    }
    // `to_string` on a Map cannot fail (no non-string keys, no NaN).
    let record = format!("{}\n", serde_json::Value::Object(line));

    let file_path = path(workspace_root);
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file_path)?;
    file.write_all(record.as_bytes())?;
    file.flush()?;
    drop(file);

    prune_if_oversized(&file_path);
    Ok(file_path)
}

/// Best-effort bound on the marker file (see [`MAX_MARKER_BYTES`]).
///
/// Rewrites via a temp file + rename, so a reader never observes a truncated
/// file. A marker appended by another process *during* the rewrite can be
/// lost; that is the accepted trade — a rare under-reported rework against a
/// file that otherwise grows forever and is re-read in full at every terminal
/// transition. Every failure is swallowed: pruning is housekeeping, and the
/// marker it follows is already durable.
fn prune_if_oversized(file_path: &Path) {
    let Ok(metadata) = std::fs::metadata(file_path) else {
        return;
    };
    if metadata.len() <= MAX_MARKER_BYTES {
        return;
    }
    let Ok(contents) = std::fs::read_to_string(file_path) else {
        return;
    };
    let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() <= KEEP_LINES {
        return;
    }
    let kept = lines[lines.len() - KEEP_LINES..].join("\n");
    let temp = file_path.with_extension("jsonl.pruning");
    if std::fs::write(&temp, format!("{kept}\n")).is_ok() {
        let _ = std::fs::rename(&temp, file_path);
    }
    let _ = std::fs::remove_file(&temp);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn marker<'a>(issue: u32, kind: &'a str) -> Marker<'a> {
        Marker {
            issue,
            kind,
            reason: None,
            classification: None,
            duration_sec: None,
        }
    }

    fn lines(dir: &TempDir) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path(dir.path()))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn appends_a_classified_line_and_creates_the_log_dir() {
        let dir = TempDir::new().unwrap();
        let written = append(
            dir.path(),
            &Marker {
                reason: Some("base branch was modified"),
                duration_sec: Some(18),
                ..marker(42, "rebase")
            },
        )
        .unwrap();
        assert_eq!(written, path(dir.path()));

        let rows = lines(&dir);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["issue"], 42);
        assert_eq!(rows[0]["kind"], "rebase");
        assert_eq!(rows[0]["reason"], "base branch was modified");
        assert_eq!(rows[0]["classification"], "environmental");
        assert_eq!(rows[0]["duration_sec"], 18);
        // `at` is what scopes a marker to a sweep's window: it must parse.
        chrono::DateTime::parse_from_rfc3339(rows[0]["at"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn appends_rather_than_replaces_and_keeps_order() {
        let dir = TempDir::new().unwrap();
        append(dir.path(), &marker(42, "rebase")).unwrap();
        append(dir.path(), &marker(43, "merge_conflict")).unwrap();
        append(dir.path(), &marker(42, "rejudge")).unwrap();
        let rows = lines(&dir);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["kind"], "rebase");
        assert_eq!(rows[1]["issue"], 43);
        assert_eq!(rows[2]["kind"], "rejudge");
        assert_eq!(rows[2]["classification"], "substantive");
    }

    #[test]
    fn an_explicit_classification_overrides_the_table() {
        let dir = TempDir::new().unwrap();
        append(
            dir.path(),
            &Marker {
                classification: Some("substantive"),
                ..marker(42, "merge_conflict")
            },
        )
        .unwrap();
        assert_eq!(lines(&dir)[0]["classification"], "substantive");
    }

    #[test]
    fn an_unknown_kind_or_classification_is_refused_not_recorded() {
        let dir = TempDir::new().unwrap();
        let err = append(dir.path(), &marker(42, "rebased")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let err = append(
            dir.path(),
            &Marker {
                classification: Some("env"),
                ..marker(42, "rebase")
            },
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        // Nothing was written: a refused vocabulary must not leave a file
        // behind that a reader would then classify by the catch-all.
        assert!(!path(dir.path()).exists());
    }

    #[test]
    fn an_absent_reason_and_duration_are_omitted_not_nulled() {
        let dir = TempDir::new().unwrap();
        append(
            dir.path(),
            &Marker {
                reason: Some("   "),
                ..marker(42, "ci_rerun")
            },
        )
        .unwrap();
        let row = &lines(&dir)[0];
        assert!(row.get("reason").is_none(), "{row}");
        assert!(row.get("duration_sec").is_none(), "{row}");
    }

    #[test]
    fn a_long_reason_is_truncated_to_one_line() {
        let dir = TempDir::new().unwrap();
        let long = "x".repeat(MAX_REASON_CHARS * 3);
        append(
            dir.path(),
            &Marker {
                reason: Some(&long),
                ..marker(42, "rebase")
            },
        )
        .unwrap();
        let raw = std::fs::read_to_string(path(dir.path())).unwrap();
        assert_eq!(raw.lines().count(), 1);
        assert_eq!(lines(&dir)[0]["reason"].as_str().unwrap().chars().count(), MAX_REASON_CHARS);
    }

    #[test]
    fn a_newline_in_a_reason_cannot_forge_a_second_marker() {
        let dir = TempDir::new().unwrap();
        append(
            dir.path(),
            &Marker {
                reason: Some("conflict\n{\"issue\":99,\"kind\":\"rejudge\"}"),
                ..marker(42, "merge_conflict")
            },
        )
        .unwrap();
        let raw = std::fs::read_to_string(path(dir.path())).unwrap();
        assert_eq!(raw.lines().count(), 1, "JSON escaping keeps it one line: {raw}");
        assert_eq!(lines(&dir)[0]["issue"], 42);
    }

    #[test]
    fn the_file_is_pruned_once_it_outgrows_the_cap() {
        let dir = TempDir::new().unwrap();
        let file_path = path(dir.path());
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        // One oversized file of well-formed markers, then one more append.
        let filler: String = (0..4000)
            .map(|i| {
                format!(
                    "{}\n",
                    serde_json::json!({"at":"2026-09-30T01:00:00Z","issue":i,"kind":"rebase"})
                )
            })
            .collect();
        std::fs::write(&file_path, &filler).unwrap();
        assert!(std::fs::metadata(&file_path).unwrap().len() > MAX_MARKER_BYTES);

        append(dir.path(), &marker(42, "rejudge")).unwrap();

        let rows = lines(&dir);
        assert_eq!(rows.len(), KEEP_LINES, "pruned to the trailing window");
        // The prune keeps the NEWEST lines, so the marker just written is the
        // one thing that can never be dropped.
        assert_eq!(rows[KEEP_LINES - 1]["kind"], "rejudge");
        assert_eq!(rows[KEEP_LINES - 1]["issue"], 42);
        assert!(!file_path.with_extension("jsonl.pruning").exists());
    }

    #[test]
    fn every_kind_in_the_vocabulary_is_classified_and_writable() {
        let dir = TempDir::new().unwrap();
        for kind in KINDS {
            assert!(
                CLASSIFICATIONS.contains(&default_classification(kind)),
                "`{kind}` classifies to a value no rollup buckets on"
            );
            append(dir.path(), &marker(42, kind)).unwrap();
        }
        assert_eq!(lines(&dir).len(), KINDS.len());
    }
}
