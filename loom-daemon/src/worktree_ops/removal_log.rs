//! The worktree-removal ledger (issue #5950).
//!
//! Loom has eight independent code paths that can delete a worktree — the
//! interactive `loom-daemon clean` pass, `clean --aggressive`, the daemon's
//! periodic [`crate::worktree_reaper`], the daemon's terminal-destroy path
//! ([`crate::terminal`]), `loom-recover-orphans`
//! ([`super::orphan_recovery`]), `merge-pr.sh`'s post-merge cleanup,
//! `worktree.sh remove`, and `agent-destroy.sh`. Each one reports its own
//! decision to its own sink (stdout for the CLI passes, `log::debug!` for the
//! reaper — which the daemon's default `info` filter drops entirely). When a
//! Builder's live worktree vanished mid-session (#5950, during issue #5919),
//! that meant there was **no single place to look** to answer "what removed
//! it?": the operator had a `clean` transcript saying `Issue #5919 is OPEN -
//! preserving`, and nothing at all from any other path.
//!
//! This module is that single place. Every Loom-owned removal appends one JSON
//! line to `<repo_root>/.loom/logs/worktree-removals.log` naming **when**,
//! **which mechanism**, **which pid**, **what path/branch**, and **on what
//! decision**. Its value is symmetric:
//!
//! - an entry identifies the responsible mechanism immediately;
//! - **no** entry is itself evidence — no Loom code path did it, so the search
//!   moves straight to host-level/manual removal (a bare `rm -rf`, a hand-run
//!   `git worktree remove`, a different checkout of this repo).
//!
//! Writes are strictly best-effort: a ledger failure must never fail (or even
//! slow) a removal, so every error is swallowed. Dry runs never write.
//! Corresponding bash-side writer: `defaults/scripts/lib/worktree-removal-log.sh`
//! (identical line format, so one `grep`/`jq` reads both).

use std::io::Write;
use std::path::Path;

/// Ledger path for a repo: `<repo_root>/.loom/logs/worktree-removals.log`.
///
/// Deliberately under `.loom/logs/` — the gitignored directory an operator
/// already greps for agent transcripts. Nothing rotates it: `loom-daemon
/// cleanup logs` delegates to `archive-logs.sh`, which only prunes
/// `.loom/logs/archive/`, not files sitting directly in `.loom/logs/`. That is
/// intentional here — the ledger's whole value is being readable *after* an
/// incident nobody noticed for a while, and its growth is bounded by how often
/// worktrees are removed at all: one ~150-byte line per removal, so even a busy
/// fleet host doing tens of removals a day writes well under a megabyte a year.
/// It should be left alone rather than added to a retention sweep.
#[must_use]
pub fn ledger_path(repo_root: &Path) -> std::path::PathBuf {
    repo_root
        .join(".loom")
        .join("logs")
        .join("worktree-removals.log")
}

/// Escape a value for embedding in the ledger's JSON line. Only the characters
/// JSON requires; paths and branch names cannot contain control characters that
/// matter here, but a backslash (Windows-style path) or quote must not break
/// the line for `jq`.
fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Render one ledger line (without its trailing newline). Split out from
/// [`record`] so the format is testable without touching the filesystem.
#[must_use]
pub fn ledger_line(
    now_rfc3339: &str,
    pid: u32,
    mechanism: &str,
    worktree_path: &Path,
    branch: Option<&str>,
    reason: &str,
) -> String {
    format!(
        r#"{{"ts":"{}","mechanism":"{}","pid":{},"worktree":"{}","branch":{},"reason":"{}"}}"#,
        json_escape(now_rfc3339),
        json_escape(mechanism),
        pid,
        json_escape(&worktree_path.display().to_string()),
        match branch {
            Some(b) => format!("\"{}\"", json_escape(b)),
            None => "null".to_string(),
        },
        json_escape(reason),
    )
}

/// Append one removal record to the repo's ledger. Best-effort: any failure
/// (unwritable directory, read-only filesystem, race) is silently ignored — the
/// removal itself is the operation that matters.
pub fn record(
    repo_root: &Path,
    mechanism: &str,
    worktree_path: &Path,
    branch: Option<&str>,
    reason: &str,
) {
    let line = ledger_line(
        &chrono::Utc::now().to_rfc3339(),
        std::process::id(),
        mechanism,
        worktree_path,
        branch,
        reason,
    );
    let path = ledger_path(repo_root);
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        // One `write_all` of a single line: the atomic-append guarantee for
        // O_APPEND writes below PIPE_BUF is what keeps concurrent removers
        // (the reaper and a CLI pass on the same repo) from interleaving.
        let _ = file.write_all(format!("{line}\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn ledger_line_is_valid_json_with_every_field() {
        let line = ledger_line(
            "2026-08-11T00:00:00+00:00",
            4242,
            "clean --aggressive",
            &PathBuf::from("/repo/.loom/worktrees/issue-5919"),
            Some("feature/issue-5919"),
            "force_override_unreachable",
        );
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("must be valid JSON");
        assert_eq!(parsed["mechanism"], "clean --aggressive");
        assert_eq!(parsed["pid"], 4242);
        assert_eq!(parsed["worktree"], "/repo/.loom/worktrees/issue-5919");
        assert_eq!(parsed["branch"], "feature/issue-5919");
        assert_eq!(parsed["reason"], "force_override_unreachable");
        assert_eq!(parsed["ts"], "2026-08-11T00:00:00+00:00");
    }

    #[test]
    fn a_branchless_worktree_records_a_null_branch() {
        let line = ledger_line(
            "2026-08-11T00:00:00+00:00",
            1,
            "clean --aggressive",
            &PathBuf::from("/repo/.loom/worktrees/pr-1"),
            None,
            "reachable_from_origin_main",
        );
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("must be valid JSON");
        assert!(parsed["branch"].is_null());
    }

    #[test]
    fn quotes_and_backslashes_cannot_break_the_line() {
        let line = ledger_line(
            "2026-08-11T00:00:00+00:00",
            1,
            "clean",
            &PathBuf::from(r#"/repo/we"ird\path"#),
            Some(r#"feature/"x""#),
            "issue_closed",
        );
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("must be valid JSON");
        assert_eq!(parsed["worktree"], r#"/repo/we"ird\path"#);
    }

    /// Two removers appending concurrently must each land a whole line — the
    /// ledger is only useful if it is greppable line-by-line.
    #[test]
    fn record_appends_one_whole_line_per_removal() {
        let dir = tempfile::tempdir().unwrap();
        record(
            dir.path(),
            "worktree_reaper",
            &PathBuf::from("/repo/.loom/worktrees/issue-1"),
            Some("feature/issue-1"),
            "issue_closed_pr_merged",
        );
        record(
            dir.path(),
            "clean --aggressive",
            &PathBuf::from("/repo/.loom/worktrees/issue-2"),
            Some("feature/issue-2"),
            "pr_merged",
        );

        let raw = std::fs::read_to_string(ledger_path(dir.path())).unwrap();
        let lines: Vec<_> = raw.lines().collect();
        assert_eq!(lines.len(), 2, "one line per removal: {raw}");
        for line in lines {
            serde_json::from_str::<serde_json::Value>(line).expect("each line parses alone");
        }
    }

    /// The ledger must never be the reason a removal fails: an unwritable
    /// location is swallowed, not propagated.
    #[test]
    fn record_is_best_effort_on_an_unwritable_repo_root() {
        let dir = tempfile::tempdir().unwrap();
        // A *file* where `.loom` must be a directory ⇒ `create_dir_all` fails.
        std::fs::write(dir.path().join(".loom"), "not a directory").unwrap();
        record(
            dir.path(),
            "clean",
            &PathBuf::from("/repo/.loom/worktrees/issue-3"),
            None,
            "issue_closed",
        );
        // No panic, and nothing written.
        assert!(!ledger_path(dir.path()).exists());
    }
}
