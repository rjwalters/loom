//! `loom-daemon clean --aggressive`: vestigial/locked-worktree cleanup.
//!
//! Rust port of `clean.py`'s aggressive-mode decision tree (see issue #3332
//! for the original rationale). Enumerates **every** `git worktree list
//! --porcelain` entry (not just `.loom/worktrees/issue-*`) and applies a
//! strict "skip beats remove" decision order, fully captured in the pure
//! [`evaluate_aggressive_candidate`] function so every branch is unit-testable
//! without touching git/gh.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use super::clean;
use super::gh;
use super::naming;

/// A single record parsed from `git worktree list --porcelain`.
#[derive(Debug, Clone, Default)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub locked: bool,
    pub bare: bool,
}

impl WorktreeInfo {
    #[must_use]
    pub fn branch_short(&self) -> Option<String> {
        self.branch
            .as_ref()
            .map(|b| b.trim_start_matches("refs/heads/").to_string())
    }
}

/// Parse `git worktree list --porcelain` into structured records. Returns an
/// empty list on any error — aggressive cleanup must fail closed (never
/// crash into an unbounded blast radius).
#[must_use]
pub fn enumerate_git_worktrees(repo_root: &Path) -> Vec<WorktreeInfo> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut worktrees = Vec::new();
    let mut current: Option<WorktreeInfo> = None;
    for raw_line in stdout.lines() {
        if raw_line.is_empty() {
            if let Some(wt) = current.take() {
                worktrees.push(wt);
            }
            continue;
        }
        if let Some(path) = raw_line.strip_prefix("worktree ") {
            if let Some(wt) = current.take() {
                worktrees.push(wt);
            }
            current = Some(WorktreeInfo {
                path: PathBuf::from(path),
                ..Default::default()
            });
            continue;
        }
        let Some(wt) = current.as_mut() else { continue };
        if let Some(head) = raw_line.strip_prefix("HEAD ") {
            wt.head = Some(head.trim().to_string());
        } else if let Some(branch) = raw_line.strip_prefix("branch ") {
            wt.branch = Some(branch.trim().to_string());
        } else if raw_line == "detached" {
            wt.detached = true;
        } else if raw_line == "bare" {
            wt.bare = true;
        } else if raw_line == "locked" || raw_line.starts_with("locked ") {
            wt.locked = true;
        }
    }
    if let Some(wt) = current.take() {
        worktrees.push(wt);
    }
    worktrees
}

/// Decision outcome for one worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Remove,
    Keep,
}

/// Why a decision was made — drives both the log line and the summary
/// counters. Mirrors the Python `reason` strings exactly (string form used
/// at the call site) via `Reason::as_str`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    BareMainWorktree,
    OpenPr,
    PrLookupFailed,
    ActiveShepherd,
    UserOwned,
    Uncommitted,
    ReachableFromOriginMain,
    /// The branch's PR is merged (including squash-merged, whose original
    /// commits are never reachable from `origin/main`) — the work is landed
    /// regardless of git reachability (#5177).
    PrMerged,
    TooRecent,
    UnreachableHead,
    ForceOverrideUnreachable,
}

impl Reason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::BareMainWorktree => "bare_main_worktree",
            Reason::OpenPr => "open_pr",
            Reason::PrLookupFailed => "pr_lookup_failed",
            Reason::ActiveShepherd => "active_shepherd",
            Reason::UserOwned => "user_owned",
            Reason::Uncommitted => "uncommitted",
            Reason::ReachableFromOriginMain => "reachable_from_origin_main",
            Reason::PrMerged => "pr_merged",
            Reason::TooRecent => "too_recent",
            Reason::UnreachableHead => "unreachable_head",
            Reason::ForceOverrideUnreachable => "force_override_unreachable",
        }
    }
}

/// Sentinel filename marking a Loom-managed worktree (issue #3334).
pub const LOOM_MANAGED_SENTINEL: &str = ".loom-managed";

/// Default minimum worktree age for `--aggressive` removal (24h in seconds).
pub const DEFAULT_AGGRESSIVE_MIN_AGE: u64 = 86400;

/// Apply the aggressive decision tree to a single worktree. Pure except for
/// the injected `head_reachable` / `has_open_pr` / `uncommitted` /
/// `age_seconds` closures/values, so the full 8-step decision order is
/// unit-testable without git/gh. Mirrors `clean.py::evaluate_aggressive_candidate`
/// step for step (first hit wins — "skip" beats "remove"):
///
/// 1. bare/main worktree -> keep
/// 2. open PR -> keep
/// 3. active spawn-loop task -> keep
/// 4. missing `.loom-managed` sentinel / non-canonical path -> keep
/// 5. uncommitted changes (unless `force`) -> keep
/// 6. HEAD reachable from origin/main -> remove
/// 7. PR merged (including squash-merged) -> remove (#5177)
/// 8. younger than `min_age_seconds` -> keep
/// 9. fallback: `force` -> remove, else keep
///
/// Step 7 is the squash-merge fix (#5177): this repo squash-merges, so a
/// merged branch's original commits are never an ancestor of `origin/main`.
/// Raw reachability (step 6) therefore cannot distinguish a safely-landed
/// squash-merged worktree from one holding genuinely unmerged work, and the
/// fallback (step 9) used to keep it forever under `UnreachableHead`. A merged
/// PR means the work IS landed regardless of reachability. Placing it AFTER
/// the uncommitted / open-PR / active-shepherd guards keeps it purely
/// **additive**: it can only turn a would-be "unreachable, keep" into a
/// remove, never override a guard that protects genuinely unmerged or
/// uncommitted work.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn evaluate_aggressive_candidate(
    wt: &WorktreeInfo,
    is_bare_or_main: bool,
    pr_lookup: Option<(bool, bool)>,
    is_active_shepherd: bool,
    is_under_loom: bool,
    has_sentinel: bool,
    is_uncommitted: bool,
    head_reachable: bool,
    pr_merged: bool,
    age_seconds: Option<u64>,
    min_age_seconds: u64,
    force: bool,
) -> (Decision, Reason) {
    if wt.bare || is_bare_or_main {
        return (Decision::Keep, Reason::BareMainWorktree);
    }

    if let Some((has_pr, ok)) = pr_lookup {
        if !ok {
            return (Decision::Keep, Reason::PrLookupFailed);
        }
        if has_pr {
            return (Decision::Keep, Reason::OpenPr);
        }
    }

    if is_active_shepherd {
        return (Decision::Keep, Reason::ActiveShepherd);
    }

    if !is_under_loom || !has_sentinel {
        return (Decision::Keep, Reason::UserOwned);
    }

    if is_uncommitted && !force {
        return (Decision::Keep, Reason::Uncommitted);
    }

    if head_reachable {
        return (Decision::Remove, Reason::ReachableFromOriginMain);
    }

    // #5177: squash-merged work is landed even though its commits are never
    // reachable from origin/main. Additive to the reachability check above.
    if pr_merged {
        return (Decision::Remove, Reason::PrMerged);
    }

    if let Some(age) = age_seconds {
        if age < min_age_seconds {
            return (Decision::Keep, Reason::TooRecent);
        }
    }

    if force {
        (Decision::Remove, Reason::ForceOverrideUnreachable)
    } else {
        (Decision::Keep, Reason::UnreachableHead)
    }
}

/// I/O glue: gather the inputs `evaluate_aggressive_candidate` needs for one
/// worktree and apply the decision. Not unit-tested directly (thin wrapper);
/// the decision tree itself is fully covered above.
fn decide_for_worktree(
    wt: &WorktreeInfo,
    repo_root: &Path,
    active_shepherds: &std::collections::HashSet<u32>,
    min_age_seconds: u64,
    force: bool,
) -> (Decision, Reason) {
    let resolved_repo = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let resolved_wt = wt.path.canonicalize().unwrap_or_else(|_| wt.path.clone());
    let is_bare_or_main = resolved_wt == resolved_repo;

    let pr_lookup = wt.branch_short().map(|b| gh::has_open_pr(repo_root, &b));

    let is_active_shepherd = wt
        .branch_short()
        .and_then(|b| naming::issue_from_branch(&b))
        .is_some_and(|n| active_shepherds.contains(&n));

    let is_under_loom = crate::worktree_root::is_worktree_path(&resolved_wt, &resolved_repo);
    let has_sentinel = resolved_wt.join(LOOM_MANAGED_SENTINEL).exists();
    let is_uncommitted = super::safety::check_uncommitted_changes(&resolved_wt);
    let head_reachable = wt
        .head
        .as_deref()
        .is_some_and(|h| is_ancestor_of_origin_main(repo_root, h));
    // #5177: only probe the forge for merged status when reachability already
    // failed — a reachable HEAD is landed anyway, so the (rate-limited) forge
    // call is pure waste there. This is the squash-merge escape hatch for the
    // otherwise-`UnreachableHead` class.
    let pr_merged = !head_reachable
        && wt
            .branch_short()
            .and_then(|b| naming::issue_from_branch(&b))
            .is_some_and(|n| pr_is_merged(repo_root, n));
    let age_seconds = worktree_age_seconds(&resolved_wt);

    evaluate_aggressive_candidate(
        wt,
        is_bare_or_main,
        pr_lookup,
        is_active_shepherd,
        is_under_loom,
        has_sentinel,
        is_uncommitted,
        head_reachable,
        pr_merged,
        age_seconds,
        min_age_seconds,
        force,
    )
}

/// Whether `issue_num`'s branch has a **merged** PR (including squash-merged).
///
/// Reuses `clean.rs`'s shared PR-merged probe (#5177) rather than building a
/// second squash-detection path. REST first — the daemon-side reaper's
/// rationale applies here too: `gh pr list` goes through the routinely-exhausted
/// GraphQL quota, while `gh api .../pulls` uses the separate, less-contended
/// REST pool — falling back to the GraphQL-backed probe only when REST cannot
/// answer.
fn pr_is_merged(repo_root: &Path, issue_num: u32) -> bool {
    let status = match clean::repo_owner_rest(repo_root)
        .map(|owner| clean::check_pr_merged_rest(repo_root, &owner, issue_num))
    {
        Some(clean::PrStatus::Unknown) | None => clean::check_pr_merged(repo_root, issue_num),
        Some(status) => status,
    };
    matches!(status, clean::PrStatus::Merged { .. })
}

fn is_ancestor_of_origin_main(repo_root: &Path, head_sha: &str) -> bool {
    if head_sha.is_empty() {
        return false;
    }
    Command::new("git")
        .args(["merge-base", "--is-ancestor", head_sha, "origin/main"])
        .current_dir(repo_root)
        .status()
        .is_ok_and(|s| s.success())
}

fn worktree_age_seconds(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|d| d.as_secs())
}

/// Summary counters for `--aggressive` (mirrors `clean.py::AggressiveStats`).
#[derive(Debug, Default)]
pub struct AggressiveStats {
    pub removed: usize,
    pub skipped_open_pr: usize,
    pub skipped_active_shepherd: usize,
    pub skipped_user_owned: usize,
    pub skipped_uncommitted: usize,
    pub skipped_too_recent: usize,
    pub skipped_unreachable: usize,
    pub skipped_locked: usize,
    pub errors: usize,
    /// One diagnostic per recorded error, in the order they occurred. Printed
    /// inline as each error happens and re-listed under the summary tally.
    pub error_details: Vec<String>,
}

impl AggressiveStats {
    /// Report a failure where it happens *and* tally it — same contract as
    /// [`clean::CleanupStats::record_error`] (#4877).
    pub fn record_error(&mut self, target: &str, operation: &str, cause: &str) {
        let line = clean::error_line(target, operation, cause);
        eprintln!("  ERROR: {line}");
        self.errors += 1;
        self.error_details.push(line);
    }
}

/// Remove one worktree in `--aggressive` mode. `Err` carries the underlying
/// cause (git's stderr) so the caller can name it in a diagnostic.
fn remove_aggressive_worktree(
    repo_root: &Path,
    wt: &WorktreeInfo,
    dry_run: bool,
) -> Result<(), String> {
    if dry_run {
        println!("Would remove worktree: {}", wt.path.display());
        if let Some(b) = wt.branch_short() {
            println!("Would delete branch: {b}");
        }
        return Ok(());
    }
    if wt.locked {
        let _ = Command::new("git")
            .args(["worktree", "unlock"])
            .arg(&wt.path)
            .current_dir(repo_root)
            .status();
    }
    let mut remove = Command::new("git");
    remove
        .args(["worktree", "remove", "--force"])
        .arg(&wt.path)
        .current_dir(repo_root);
    clean::run_checked(remove)?;
    println!("  Removed worktree: {}", wt.path.display());
    if let Some(b) = wt.branch_short() {
        let _ = Command::new("git")
            .args(["branch", "-D", &b])
            .current_dir(repo_root)
            .status();
    }
    Ok(())
}

/// Run the full `--aggressive` pass. Mirrors `clean.py::clean_aggressive`.
pub fn clean_aggressive(
    repo_root: &Path,
    dry_run: bool,
    force: bool,
    min_age_seconds: u64,
) -> AggressiveStats {
    let mut stats = AggressiveStats::default();
    let active_shepherds = super::liveness::active_spawn_loop_issues(repo_root);

    let worktrees = enumerate_git_worktrees(repo_root);
    if worktrees.is_empty() {
        println!("No worktrees enumerated from `git worktree list`");
        return stats;
    }

    for wt in &worktrees {
        let label = match wt.branch_short() {
            Some(b) => format!("{} [{}]", wt.path.display(), b),
            None if wt.detached => format!("{} [detached]", wt.path.display()),
            None => wt.path.display().to_string(),
        };

        let (decision, reason) =
            decide_for_worktree(wt, repo_root, &active_shepherds, min_age_seconds, force);

        match decision {
            Decision::Keep => {
                match reason {
                    Reason::BareMainWorktree => {
                        stats.skipped_locked += 1;
                        println!("  Skip (main worktree): {label}");
                    }
                    Reason::OpenPr | Reason::PrLookupFailed => {
                        stats.skipped_open_pr += 1;
                        println!("  Skip ({}): {label}", reason.as_str());
                    }
                    Reason::ActiveShepherd => {
                        stats.skipped_active_shepherd += 1;
                        println!("  Skip (active shepherd): {label}");
                    }
                    Reason::UserOwned => {
                        stats.skipped_user_owned += 1;
                        println!("  Skip (user-owned / no .loom-managed sentinel): {label}");
                    }
                    Reason::Uncommitted => {
                        stats.skipped_uncommitted += 1;
                        println!("  Skip (uncommitted changes; pass --force to override): {label}");
                    }
                    Reason::TooRecent => {
                        stats.skipped_too_recent += 1;
                        println!("  Skip (younger than min-age): {label}");
                    }
                    Reason::UnreachableHead => {
                        stats.skipped_unreachable += 1;
                        println!("  Skip (HEAD not on origin/main — would lose work): {label}");
                        if let Some(h) = &wt.head {
                            println!(
                                "    HEAD={} (recoverable via `git reflog`)",
                                &h[..h.len().min(12)]
                            );
                        }
                    }
                    Reason::ReachableFromOriginMain
                    | Reason::PrMerged
                    | Reason::ForceOverrideUnreachable => {
                        // Unreachable in Keep branch — defensive, never hit.
                        stats.record_error(
                            &label,
                            "internal decision check",
                            &format!(
                                "Keep decision paired with removal reason `{}` - this is a bug in \
                                 decide_for_worktree; the worktree was left untouched",
                                reason.as_str()
                            ),
                        );
                    }
                }
                continue;
            }
            Decision::Remove => {
                if reason == Reason::ForceOverrideUnreachable {
                    let head_prefix = wt
                        .head
                        .as_deref()
                        .map(|h| &h[..h.len().min(12)])
                        .unwrap_or("?");
                    println!("  Removing despite unreachable HEAD ({head_prefix}): {label}");
                } else {
                    println!("  Remove ({}): {label}", reason.as_str());
                }
                match remove_aggressive_worktree(repo_root, wt, dry_run) {
                    Ok(()) => stats.removed += 1,
                    Err(cause) => {
                        stats.record_error(&label, "git worktree remove --force", &cause);
                    }
                }
            }
        }
    }

    if !dry_run {
        let _ = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(repo_root)
            .status();
    }

    stats
}

/// Render `AggressiveStats` in the same shape as `clean.py::print_aggressive_summary`.
pub fn print_aggressive_summary(stats: &AggressiveStats, dry_run: bool) {
    println!();
    println!("========================================");
    println!("  Aggressive Cleanup Summary");
    println!("========================================");
    println!();
    if dry_run {
        println!("  Would remove: {} worktree(s)", stats.removed);
    } else {
        println!("  Removed: {} worktree(s)", stats.removed);
    }
    if stats.skipped_open_pr > 0 {
        println!("  Skipped (open PR / lookup failed): {}", stats.skipped_open_pr);
    }
    if stats.skipped_active_shepherd > 0 {
        println!("  Skipped (active shepherd): {}", stats.skipped_active_shepherd);
    }
    if stats.skipped_user_owned > 0 {
        println!(
            "  Skipped (user-owned / no .loom-managed sentinel): {}",
            stats.skipped_user_owned
        );
    }
    if stats.skipped_uncommitted > 0 {
        println!("  Skipped (uncommitted changes): {}", stats.skipped_uncommitted);
    }
    if stats.skipped_too_recent > 0 {
        println!("  Skipped (younger than min-age): {}", stats.skipped_too_recent);
    }
    if stats.skipped_unreachable > 0 {
        println!("  Skipped (HEAD unreachable — would lose work): {}", stats.skipped_unreachable);
    }
    if stats.skipped_locked > 0 {
        println!("  Skipped (main worktree): {}", stats.skipped_locked);
    }
    if stats.errors > 0 {
        println!("  Errors: {}", stats.errors);
        for detail in &stats.error_details {
            println!("    - {detail}");
        }
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wt() -> WorktreeInfo {
        WorktreeInfo {
            path: PathBuf::from("/repo/.loom/worktrees/issue-42"),
            head: Some("abc123".to_string()),
            branch: Some("refs/heads/feature/issue-42".to_string()),
            detached: false,
            locked: false,
            bare: false,
        }
    }

    /// #4877: an aggressive-mode failure must name the worktree, the
    /// operation, and git's own message — not just bump `Errors: N`.
    #[test]
    fn record_error_names_worktree_operation_and_cause() {
        let mut stats = AggressiveStats::default();
        stats.record_error(
            "/repo/.loom/worktrees/issue-42 [feature/issue-42]",
            "git worktree remove --force",
            "fatal: validation failed, cannot remove working tree",
        );
        assert_eq!(stats.errors, 1);
        let detail = &stats.error_details[0];
        assert!(detail.contains("issue-42"), "must name the worktree: {detail}");
        assert!(detail.contains("git worktree remove --force"), "must name the op: {detail}");
        assert!(detail.contains("validation failed"), "must carry git's error: {detail}");
    }

    #[test]
    fn bare_worktree_is_always_kept() {
        let mut w = wt();
        w.bare = true;
        let (d, r) = evaluate_aggressive_candidate(
            &w, false, None, false, true, true, false, true, false, None, 86400, false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::BareMainWorktree);
    }

    #[test]
    fn open_pr_beats_everything_else() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((true, true)),
            false,
            true,
            true,
            false,
            true,
            false,
            None,
            86400,
            true, // even with force
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::OpenPr);
    }

    #[test]
    fn failed_pr_lookup_fails_closed() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, false)),
            false,
            true,
            true,
            false,
            true,
            false,
            None,
            86400,
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::PrLookupFailed);
    }

    #[test]
    fn active_shepherd_is_kept() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, true)),
            true,
            true,
            true,
            false,
            true,
            false,
            None,
            86400,
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::ActiveShepherd);
    }

    #[test]
    fn missing_sentinel_is_user_owned() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, true)),
            false,
            true,
            false,
            false,
            true,
            false,
            None,
            86400,
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::UserOwned);
    }

    #[test]
    fn outside_loom_root_is_user_owned_even_with_sentinel() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, true)),
            false,
            false,
            true,
            false,
            true,
            false,
            None,
            86400,
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::UserOwned);
    }

    #[test]
    fn uncommitted_changes_are_kept_unless_forced() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, true)),
            false,
            true,
            true,
            true,
            true,
            false,
            None,
            86400,
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::Uncommitted);

        let (d2, _) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, true)),
            false,
            true,
            true,
            true,
            true,
            false,
            None,
            86400,
            true,
        );
        assert_eq!(d2, Decision::Remove);
    }

    #[test]
    fn reachable_head_is_removed_regardless_of_age() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, true)),
            false,
            true,
            true,
            false,
            true,
            false,
            Some(1), // 1 second old — would fail the age gate if reached
            86400,
            false,
        );
        assert_eq!(d, Decision::Remove);
        assert_eq!(r, Reason::ReachableFromOriginMain);
    }

    #[test]
    fn unreachable_and_too_recent_is_kept() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, true)),
            false,
            true,
            true,
            false,
            false,
            false,
            Some(10),
            86400,
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::TooRecent);
    }

    #[test]
    fn unreachable_and_old_enough_is_kept_without_force() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, true)),
            false,
            true,
            true,
            false,
            false,
            false,
            Some(999_999),
            86400,
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::UnreachableHead);
    }

    #[test]
    fn unreachable_and_old_enough_is_removed_with_force() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, true)),
            false,
            true,
            true,
            false,
            false,
            false,
            Some(999_999),
            86400,
            true,
        );
        assert_eq!(d, Decision::Remove);
        assert_eq!(r, Reason::ForceOverrideUnreachable);
    }

    /// #5177 AC1: a squash-merged worktree has an unreachable HEAD (its commits
    /// are never an ancestor of origin/main) yet its PR is merged — it must be
    /// removed, not retained under `UnreachableHead`.
    #[test]
    fn unreachable_but_pr_merged_is_removed() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, true)), // no OPEN pr, lookup ok
            false,
            true,
            true,
            false,         // not uncommitted
            false,         // HEAD not reachable (squash-merged)
            true,          // ...but the PR is merged
            Some(999_999), // old enough that the age gate would otherwise not matter
            86400,
            false, // no --force needed
        );
        assert_eq!(d, Decision::Remove);
        assert_eq!(r, Reason::PrMerged);
    }

    /// #5177 AC2: the merged-PR check is ADDITIVE — it must never override the
    /// uncommitted-work guard. Uncommitted changes win even when the PR merged.
    #[test]
    fn uncommitted_is_kept_even_when_pr_merged() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, true)),
            false,
            true,
            true,
            true,  // uncommitted work present
            false, // HEAD not reachable
            true,  // PR merged
            None,
            86400,
            false, // not forced
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::Uncommitted);
    }

    /// #5177 AC2: an open PR still beats the merged-PR path — a worktree whose
    /// branch has an OPEN pr is kept regardless of any merged-status probe.
    #[test]
    fn open_pr_still_beats_pr_merged() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((true, true)), // OPEN pr present
            false,
            true,
            true,
            false,
            false,
            true, // even if a merged-status probe somehow also said yes
            None,
            86400,
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::OpenPr);
    }

    #[test]
    fn enumerate_git_worktrees_returns_empty_on_non_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(enumerate_git_worktrees(dir.path()).is_empty());
    }
}
