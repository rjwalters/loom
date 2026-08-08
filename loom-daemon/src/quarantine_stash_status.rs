//! Fleet-wide, per-repo `git stash` visibility for `loom-daemon status`
//! (Issue #5692, sub-issue of #5690).
//!
//! `loom-quarantine:`-labeled stashes (created by
//! `check-main-clean.sh --quarantine` when it rescues contaminated
//! main-worktree changes rather than discarding them) were previously
//! invisible fleet-wide: the only existing surface,
//! `check-quarantine-stashes.sh` (#5185), is advisory and scoped to whichever
//! single repo/host it happens to run from. #5690's fleet audit had to SSH
//! into three hosts by hand to count 148 stashes accumulated over 12 days.
//!
//! This module builds on the same `refs/stash` reflog enumeration
//! `check-quarantine-stashes.sh` already uses (`git stash list`, which reads
//! `refs/stash`'s reflog — shared across every linked worktree of a repo, not
//! per-worktree, so counting from any one checkout is representative of the
//! whole repo) — the daemon-side *aggregation* into a per-repo summary is the
//! new part, not the underlying stash-discovery walk.

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

/// The label substring `check-main-clean.sh --quarantine` stamps into a
/// rescue stash's message (mirrors `check-quarantine-stashes.sh`'s (#5185)
/// own `grep 'loom-quarantine:'` filter and the `.loom/logs/main-quarantine.log`
/// `stash_message` field).
pub const QUARANTINE_STASH_LABEL: &str = "loom-quarantine:";

/// Aggregated stash counts for one managed repo (Issue #5692), as reported by
/// [`collect_stash_summary`] and rendered by `loom-daemon status`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StashSummary {
    /// Total entries currently in this repo's `refs/stash`.
    pub total_count: usize,
    /// Of `total_count`, how many carry the [`QUARANTINE_STASH_LABEL`] —
    /// the subset `check-main-clean.sh --quarantine` created, as opposed to
    /// an ad-hoc `git stash` (a Judge park, an Auditor drift-stash, etc.).
    pub quarantine_count: usize,
    /// Age, in whole seconds, of the OLDEST entry in `refs/stash` (any
    /// label) as of collection time — `None` when there are no stashes at
    /// all.
    pub oldest_stash_age_secs: Option<u64>,
}

/// One parsed `git stash list --format='%ct|%gs'` reflog line. Kept separate
/// from [`StashSummary`] so [`summarize`] stays pure (no wall-clock
/// dependency) and independently unit-testable from the `git`-invoking
/// [`collect_stash_summary`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct StashEntry {
    /// `%ct` — the reflog entry's committer-date, unix epoch seconds.
    committed_at_epoch: i64,
    /// `%gs` — the reflog subject, e.g. `"On main: loom-quarantine: issue=5388"`.
    subject: String,
}

/// Parse `git stash list --format='%ct|%gs'` stdout into entries. A
/// malformed line (missing the `|` separator, or an unparseable epoch) is
/// skipped rather than failing the whole parse — one corrupt reflog line
/// must not blank out the rest of a repo's summary.
fn parse_stash_list(stdout: &str) -> Vec<StashEntry> {
    stdout
        .lines()
        .filter_map(|line| {
            let (epoch_str, subject) = line.split_once('|')?;
            let committed_at_epoch = epoch_str.trim().parse::<i64>().ok()?;
            Some(StashEntry {
                committed_at_epoch,
                subject: subject.to_string(),
            })
        })
        .collect()
}

/// Reduce parsed stash entries into the aggregate [`StashSummary`] as of
/// `now_epoch` (unix seconds) — pure, no I/O, so it is unit-testable
/// independent of wall-clock time and without a real git repo.
fn summarize(entries: &[StashEntry], now_epoch: i64) -> StashSummary {
    let total_count = entries.len();
    let quarantine_count = entries
        .iter()
        .filter(|e| e.subject.contains(QUARANTINE_STASH_LABEL))
        .count();
    let oldest_stash_age_secs = entries
        .iter()
        .map(|e| e.committed_at_epoch)
        .min()
        .map(|oldest_epoch| now_epoch.saturating_sub(oldest_epoch).max(0) as u64);
    StashSummary {
        total_count,
        quarantine_count,
        oldest_stash_age_secs,
    }
}

/// Collect `root`'s stash summary by shelling out to `git stash list`
/// (Issue #5692) — the same `refs/stash` reflog `check-quarantine-stashes.sh`
/// (#5185) reads, aggregated instead of printed as a human warning.
///
/// Best-effort: `root` not being a git repo, having zero stashes, or `git`
/// itself failing/being absent all degrade to the zero-valued
/// [`StashSummary::default`] rather than propagating an error — one repo's
/// stash read must never block `loom-daemon status` for its siblings.
#[must_use]
pub fn collect_stash_summary(root: &Path) -> StashSummary {
    let output = match Command::new("git")
        .args(["stash", "list", "--format=%ct|%gs"])
        .current_dir(root)
        .output()
    {
        Ok(out) if out.status.success() => out,
        _ => return StashSummary::default(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let entries = parse_stash_list(&stdout);
    let now_epoch = chrono::Utc::now().timestamp();
    summarize(&entries, now_epoch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(committed_at_epoch: i64, subject: &str) -> StashEntry {
        StashEntry {
            committed_at_epoch,
            subject: subject.to_string(),
        }
    }

    #[test]
    fn parse_stash_list_extracts_epoch_and_subject() {
        let stdout = "1785907087|On main: loom-quarantine: issue=5388\n\
                       1786080259|On main: auditor: stray package-lock.json diff before sync\n";
        let entries = parse_stash_list(stdout);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].committed_at_epoch, 1785907087);
        assert_eq!(entries[0].subject, "On main: loom-quarantine: issue=5388");
        assert_eq!(entries[1].committed_at_epoch, 1786080259);
    }

    #[test]
    fn parse_stash_list_skips_malformed_lines() {
        // No `|` separator, and a non-numeric epoch — both must be skipped
        // without panicking or dropping the well-formed sibling line.
        let stdout = "not-a-valid-line\n\
                       not-a-number|On main: loom-quarantine: issue=1\n\
                       1785907087|On main: loom-quarantine: issue=2\n";
        let entries = parse_stash_list(stdout);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].committed_at_epoch, 1785907087);
    }

    #[test]
    fn parse_stash_list_handles_empty_output() {
        assert!(parse_stash_list("").is_empty());
    }

    #[test]
    fn summarize_counts_total_and_quarantine_labeled_separately() {
        let entries = vec![
            entry(100, "On main: loom-quarantine: issue=1"),
            entry(200, "On main: auditor: stray diff"),
            entry(300, "On main: loom-quarantine: run=abc issue=2"),
        ];
        let summary = summarize(&entries, 1000);
        assert_eq!(summary.total_count, 3);
        assert_eq!(summary.quarantine_count, 2, "only the loom-quarantine: labeled entries count");
    }

    #[test]
    fn summarize_reports_age_of_the_oldest_entry_regardless_of_label() {
        // Reflog entries are newest-first in `git stash list`, but the oldest
        // by committed-at epoch is entry index 2 here (epoch 100) — the
        // summary must find it independent of input ordering.
        let entries = vec![
            entry(500, "On main: loom-quarantine: issue=1"),
            entry(300, "On main: auditor: stray diff"),
            entry(100, "On main: loom-quarantine: issue=2"),
        ];
        let summary = summarize(&entries, 1000);
        assert_eq!(summary.oldest_stash_age_secs, Some(900));
    }

    #[test]
    fn summarize_of_empty_entries_reports_no_oldest_age() {
        let summary = summarize(&[], 1000);
        assert_eq!(summary.total_count, 0);
        assert_eq!(summary.quarantine_count, 0);
        assert_eq!(summary.oldest_stash_age_secs, None);
    }

    #[test]
    fn summarize_clamps_a_future_committed_at_to_zero_age_rather_than_underflowing() {
        // Clock skew between the recording machine and this one could put a
        // reflog timestamp momentarily in the future relative to `now_epoch`
        // — `saturating_sub` must clamp to 0, never panic/wrap on unsigned
        // conversion.
        let entries = vec![entry(2000, "On main: loom-quarantine: issue=1")];
        let summary = summarize(&entries, 1000);
        assert_eq!(summary.oldest_stash_age_secs, Some(0));
    }

    /// End-to-end: a real git repo, one ordinary stash and one
    /// `loom-quarantine:`-labeled stash, collected via [`collect_stash_summary`]
    /// (not just the pure `parse`/`summarize` halves above).
    #[test]
    fn collect_stash_summary_counts_real_stashes_in_a_temp_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };

        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(root.join("README.md"), "hello\n").expect("write README");
        git(&["add", "README.md"]);
        git(&["commit", "-q", "-m", "initial"]);

        // First stash: an ordinary (non-quarantine) WIP stash.
        std::fs::write(root.join("README.md"), "hello\nordinary wip\n").expect("edit");
        git(&["stash", "push", "-m", "ordinary wip, not a quarantine"]);

        // Second stash: a loom-quarantine:-labeled rescue stash.
        std::fs::write(root.join("README.md"), "hello\nquarantined change\n").expect("edit");
        git(&["stash", "push", "-m", "loom-quarantine: issue=9999"]);

        let summary = collect_stash_summary(root);
        assert_eq!(summary.total_count, 2);
        assert_eq!(summary.quarantine_count, 1);
        let age = summary.oldest_stash_age_secs.expect("at least one stash");
        // The oldest stash (the "ordinary wip" one) was just created — its
        // age must be small (well under a minute), not `None` or huge.
        assert!(age < 60, "expected a small age for a just-created stash, got {age}s");
    }

    /// A directory that is not a git repo at all must degrade to the
    /// zero-valued default rather than erroring `loom-daemon status`.
    #[test]
    fn collect_stash_summary_on_a_non_git_directory_returns_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let summary = collect_stash_summary(dir.path());
        assert_eq!(summary, StashSummary::default());
    }
}
