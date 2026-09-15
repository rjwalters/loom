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

// ============================================================================
// Origin classification (#5512) — labels every `refs/stash` entry, not just
// the `loom-quarantine:`-labeled subset.
// ============================================================================
//
// #5693's `stash_retirement` module already classifies the `loom-quarantine:`
// subset for retirement. But #5690's fleet audit and the #6129/#6162 incident
// (a half-applied pop of an *unlabeled* stash broke `spawn-claude.sh`) both
// found real content sitting on the same shared `refs/stash` stack wearing no
// recognizable label at all: an operator's pre-reinstall preservation, an
// agent's ad-hoc WIP, an Auditor drift-shelf entry. None of those were ever
// counted anywhere. This section labels every entry by *origin* — purely from
// the reflog subject text already read by [`collect_stash_summary`], so it
// costs no extra `git` shell-outs — so `status` can surface them without
// pretending to know whether their content is safe to lose (only
// [`is_presumed_recoverable`] makes that call, and only for `Auditor`).

/// Loom-owned Auditor drift-shelf message patterns. Mirrors
/// `check-main-clean.sh`'s own `LOOM_STASH_PATTERNS` `"auditor-drift"`
/// producer (`auditor-tmp-drift-stash-<epoch>`), plus the broader `auditor:`
/// message prefix an Auditor role stash may carry.
const AUDITOR_STASH_PATTERNS: &[&str] = &["auditor-tmp-drift-stash-", "auditor-drift", "auditor:"];

/// `install.sh --quick`'s pre-reinstall preservation message
/// (`"loom-install: preserving user changes before --quick reinstall"`) — the
/// one already-documented "operator pre-resync stash" convention.
const OPERATOR_STASH_PATTERNS: &[&str] = &["loom-install:"];

/// Origin of one `refs/stash` entry, inferred from its reflog subject.
/// Deliberately conservative: everything that is not provably one of the
/// four named producers falls to [`StashOrigin::Unknown`] rather than being
/// guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StashOrigin {
    /// `check-main-clean.sh --quarantine`'s rescue stash — see
    /// [`QUARANTINE_STASH_LABEL`]. Owns its own lifecycle
    /// (`loom-daemon stashes list`/`retire`, #5693) — never touched by the
    /// non-quarantine surfacing this module adds.
    Quarantine,
    /// Auditor's temporary drift shelf — see [`AUDITOR_STASH_PATTERNS`].
    /// Machine-computed, regenerable content by construction: this is the
    /// only origin [`is_presumed_recoverable`] treats as safe to lose.
    Auditor,
    /// A human operator's own stash — `install.sh --quick`'s pre-reinstall
    /// preservation ([`OPERATOR_STASH_PATTERNS`]), or (per `judge.md`'s "the
    /// main checkout's stash stack is operator-owned") any entry recorded
    /// against the `main` branch that isn't one of the other recognized
    /// producers.
    Operator,
    /// An agent's ad-hoc WIP — any entry recorded against a branch other
    /// than `main` (every Loom-managed worktree checks out its own
    /// `feature/issue-<N>`-style branch, never `main`), whether it carries
    /// git's own auto-generated `"WIP on <branch>: ..."` message (no custom
    /// `-m`) or a custom one that matches no other recognized producer
    /// (e.g. a Judge's ad-hoc `"judge-<N>: parking ..."` park stash). This
    /// is exactly the shape `builder.md`'s "Never use bare `git stash` for
    /// ad-hoc WIP" warns against.
    AgentWip,
    /// None of the above — the branch could not be determined from the
    /// reflog subject at all (a malformed or unexpected format). Never
    /// treated as safe-by-default.
    Unknown,
}

impl StashOrigin {
    /// Short, stable label for human-readable output (CLI rendering).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            StashOrigin::Quarantine => "quarantine",
            StashOrigin::Auditor => "auditor",
            StashOrigin::Operator => "operator",
            StashOrigin::AgentWip => "agent-wip",
            StashOrigin::Unknown => "unknown",
        }
    }
}

/// Extract the branch name recorded in a stash reflog subject's `"On
/// <branch>: ..."` / `"WIP on <branch>: ..."` prefix (git always records
/// one of these two shapes — the former after a custom `-m`, the latter as
/// its own auto-generated default), or `None` if the subject matches
/// neither shape.
fn stash_branch(subject: &str) -> Option<&str> {
    let rest = subject
        .strip_prefix("WIP on ")
        .or_else(|| subject.strip_prefix("On "))?;
    let (branch, _) = rest.split_once(": ")?;
    Some(branch)
}

/// Classify one `refs/stash` reflog subject by origin. `subject` is the raw
/// `%gs` text, including its `"On <branch>: "` / `"WIP on <branch>: "`
/// prefix — callers must NOT pre-strip it, since the prefix shape itself
/// (which branch the entry was pushed against) is part of the
/// [`StashOrigin::AgentWip`] vs [`StashOrigin::Operator`] signal.
#[must_use]
pub fn classify_stash_origin(subject: &str) -> StashOrigin {
    if subject.contains(QUARANTINE_STASH_LABEL) {
        return StashOrigin::Quarantine;
    }
    if AUDITOR_STASH_PATTERNS.iter().any(|p| subject.contains(p)) {
        return StashOrigin::Auditor;
    }
    if OPERATOR_STASH_PATTERNS.iter().any(|p| subject.contains(p)) {
        return StashOrigin::Operator;
    }
    match stash_branch(subject) {
        Some("main") => StashOrigin::Operator,
        Some(_) => StashOrigin::AgentWip,
        None => StashOrigin::Unknown,
    }
}

/// Whether `origin` is one this module already knows is regenerable /
/// reproducible without the stash — the only origin treated as "recoverable
/// by construction" rather than presumed-precious. Deliberately narrow:
/// `Operator`, `AgentWip`, and `Unknown` may each be the only copy of real,
/// uncommitted work — exactly the shape that hid the #6129 quiesce work
/// behind an "it's just a stash" shrug until its half-applied pop broke
/// `spawn-claude.sh` (#6162). `Quarantine` is excluded from this question
/// entirely — it has its own two-condition retirement classifier
/// (`stash_retirement::classify_stash`), not this one-bit heuristic.
#[must_use]
pub fn is_presumed_recoverable(origin: StashOrigin) -> bool {
    matches!(origin, StashOrigin::Auditor)
}

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
    /// #5512: of the non-quarantine entries, how many are NOT
    /// [`is_presumed_recoverable`] — i.e. every origin except `Auditor`'s
    /// regenerable drift shelf. A nonzero count means there is at least one
    /// stash on this repo's stack whose content exists nowhere else that
    /// `status` can see.
    pub non_quarantine_unrecoverable_count: usize,
    /// Age, in whole seconds, of the OLDEST entry counted in
    /// `non_quarantine_unrecoverable_count` — `None` when that count is 0.
    pub non_quarantine_unrecoverable_oldest_age_secs: Option<u64>,
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
    let oldest_stash_age_secs = entries
        .iter()
        .map(|e| e.committed_at_epoch)
        .min()
        .map(|oldest_epoch| now_epoch.saturating_sub(oldest_epoch).max(0) as u64);

    // #5512: classify every entry by origin — purely from the `%gs` text
    // already parsed above, no extra `git` shell-outs — to derive both
    // `quarantine_count` (unchanged in outcome from the old plain
    // `.contains(QUARANTINE_STASH_LABEL)` check, now routed through the same
    // classifier as everything else so the two can never drift apart) and
    // the new non-quarantine "unrecoverable" count/age.
    let mut quarantine_count = 0usize;
    let mut non_quarantine_unrecoverable_count = 0usize;
    let mut non_quarantine_unrecoverable_oldest_epoch: Option<i64> = None;
    for e in entries {
        let origin = classify_stash_origin(&e.subject);
        if origin == StashOrigin::Quarantine {
            quarantine_count += 1;
            continue;
        }
        if is_presumed_recoverable(origin) {
            continue;
        }
        non_quarantine_unrecoverable_count += 1;
        non_quarantine_unrecoverable_oldest_epoch =
            Some(match non_quarantine_unrecoverable_oldest_epoch {
                Some(cur) => cur.min(e.committed_at_epoch),
                None => e.committed_at_epoch,
            });
    }
    let non_quarantine_unrecoverable_oldest_age_secs = non_quarantine_unrecoverable_oldest_epoch
        .map(|oldest_epoch| now_epoch.saturating_sub(oldest_epoch).max(0) as u64);

    StashSummary {
        total_count,
        quarantine_count,
        oldest_stash_age_secs,
        non_quarantine_unrecoverable_count,
        non_quarantine_unrecoverable_oldest_age_secs,
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

    // ---------- origin classification (#5512) ----------

    #[test]
    fn classify_stash_origin_recognizes_quarantine() {
        assert_eq!(
            classify_stash_origin("On main: loom-quarantine: run=sweep-1 issue=5388"),
            StashOrigin::Quarantine
        );
        // The `issue=`/`run=` tokens are irrelevant to origin — presence of
        // the label substring alone is decisive, matching
        // `stash_retirement::QUARANTINE_STASH_LABEL`'s own contract.
        assert_eq!(
            classify_stash_origin("On main: loom-quarantine: unattributed"),
            StashOrigin::Quarantine
        );
    }

    #[test]
    fn classify_stash_origin_recognizes_auditor_drift_shelf() {
        assert_eq!(
            classify_stash_origin("On main: auditor-tmp-drift-stash-1785796450"),
            StashOrigin::Auditor
        );
        assert_eq!(
            classify_stash_origin("On main: auditor: stray package-lock.json diff before sync"),
            StashOrigin::Auditor
        );
    }

    #[test]
    fn classify_stash_origin_recognizes_operator_install_preservation() {
        assert_eq!(
            classify_stash_origin(
                "On main: loom-install: preserving user changes before --quick reinstall"
            ),
            StashOrigin::Operator
        );
    }

    #[test]
    fn classify_stash_origin_treats_any_main_branch_entry_as_operator_by_default() {
        // judge.md: "the main checkout's stash stack is operator-owned" —
        // an unrecognized custom message pushed against main falls to
        // Operator, not Unknown.
        assert_eq!(
            classify_stash_origin("On main: pre-test-merge baseline, will restore after"),
            StashOrigin::Operator
        );
        // Git's own default message (no custom `-m`) on main is the same
        // call: a human ran a bare `git stash` in the primary clone.
        assert_eq!(
            classify_stash_origin("WIP on main: abc1234 fix: something"),
            StashOrigin::Operator
        );
    }

    #[test]
    fn classify_stash_origin_treats_any_non_main_branch_entry_as_agent_wip() {
        // Git's own default message on an issue worktree branch — exactly
        // the shape `builder.md`'s "Never use bare git stash" rule warns
        // against.
        assert_eq!(
            classify_stash_origin("WIP on feature/issue-5654: 0e703af1 docs: update WORK_LOG"),
            StashOrigin::AgentWip
        );
        // A custom message on a non-main branch is still agent territory —
        // no agent-owned worktree checks out `main`.
        assert_eq!(
            classify_stash_origin(
                "On feature/issue-5577: judge-5584: parking pre-existing staged reversion"
            ),
            StashOrigin::AgentWip
        );
    }

    #[test]
    fn classify_stash_origin_falls_back_to_unknown_when_no_branch_can_be_parsed() {
        assert_eq!(
            classify_stash_origin("not a recognizable stash subject at all"),
            StashOrigin::Unknown
        );
    }

    #[test]
    fn is_presumed_recoverable_is_true_only_for_auditor() {
        assert!(is_presumed_recoverable(StashOrigin::Auditor));
        assert!(!is_presumed_recoverable(StashOrigin::Quarantine));
        assert!(!is_presumed_recoverable(StashOrigin::Operator));
        assert!(!is_presumed_recoverable(StashOrigin::AgentWip));
        assert!(!is_presumed_recoverable(StashOrigin::Unknown));
    }

    // ---------- non-quarantine "unrecoverable" surfacing (#5512) ----------

    #[test]
    fn summarize_counts_non_quarantine_unrecoverable_entries_excluding_auditor() {
        let entries = vec![
            entry(100, "On main: loom-quarantine: issue=1"), // quarantine — excluded
            entry(200, "On main: auditor-tmp-drift-stash-200"), // auditor — presumed recoverable
            entry(300, "On main: loom-install: preserving user changes before --quick reinstall"), // operator
            entry(400, "WIP on feature/issue-42: abc1234 docs: x"), // agent-wip
        ];
        let summary = summarize(&entries, 1000);
        assert_eq!(summary.total_count, 4);
        assert_eq!(summary.quarantine_count, 1);
        // operator (300) + agent-wip (400) count; auditor (200) does not.
        assert_eq!(summary.non_quarantine_unrecoverable_count, 2);
        // Oldest of the counted two is the operator entry at epoch 300.
        assert_eq!(summary.non_quarantine_unrecoverable_oldest_age_secs, Some(700));
    }

    #[test]
    fn summarize_reports_no_non_quarantine_unrecoverable_oldest_age_when_count_is_zero() {
        let entries = vec![
            entry(100, "On main: loom-quarantine: issue=1"),
            entry(200, "On main: auditor-tmp-drift-stash-200"),
        ];
        let summary = summarize(&entries, 1000);
        assert_eq!(summary.non_quarantine_unrecoverable_count, 0);
        assert_eq!(summary.non_quarantine_unrecoverable_oldest_age_secs, None);
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
