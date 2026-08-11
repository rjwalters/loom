//! `loom-daemon clean --aggressive`: vestigial/locked-worktree cleanup.
//!
//! Rust port of `clean.py`'s aggressive-mode decision tree (see issue #3332
//! for the original rationale). Enumerates **every** `git worktree list
//! --porcelain` entry (not just `.loom/worktrees/issue-*`) and applies a
//! strict "skip beats remove" decision order, fully captured in the pure
//! [`evaluate_aggressive_candidate`] function so every branch is unit-testable
//! without touching git/gh.
//!
//! # Issue-open state (#5950)
//!
//! Until #5950 this was the **only** worktree-removal decision surface in Loom
//! that never consulted issue-open state. [`clean::classify_worktree`]'s
//! ordinary path (used by both the interactive CLI and
//! [`crate::worktree_reaper`]) refuses to touch a worktree whose issue is not
//! `CLOSED` — that is the gate whose `Issue #N is OPEN - preserving` line an
//! operator sees. Aggressive mode reached its own removal decisions from open
//! PR + uncommitted-changes + reachability alone, so a `feature/issue-N`
//! worktree belonging to a **live Builder session on an open issue** was
//! removable by it, with the same command in the same shell having just
//! printed a preservation decision for that exact issue from the ordinary
//! pass. Two further facts made that reachable in practice:
//!
//! - the `active_shepherd` gate only protects issues holding a
//!   `.loom/locks/issue-<N>/` claim-lock, which **only daemon-dispatched
//!   sweeps take** — a manually run `/loom:sweep` / Builder session has none;
//! - aggressive mode deliberately overrides `.loom-in-use` markers and the
//!   process-table guard (see the CLI banner), so neither of those covers it.
//!
//! The gate added in [`evaluate_aggressive_candidate`] closes that: an open
//! (or `UNKNOWN`) issue keeps its worktree **unless the removal cannot lose
//! anything** — the working tree is clean *and* the work is landed (HEAD
//! reachable from `origin/main`, or a merged PR). That carve-out is deliberate
//! and is what keeps aggressive mode useful: partial-increment slices
//! (`Part of #N`) merge while the family issue #N stays open forever, and
//! those worktrees must still be reclaimable. Worktrees with no `issue-N`
//! branch (detached, `pr-NNNN`, arbitrary user paths) have no issue state to
//! consult and are unaffected.

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
    /// The worktree's issue is not `CLOSED` and the removal is not backed by
    /// landed work — a Builder may be mid-session on it (#5950).
    IssueStillOpen,
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
            Reason::IssueStillOpen => "issue_still_open",
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
/// 6. issue not `CLOSED` and the removal is not backed by landed work -> keep (#5950)
/// 7. HEAD reachable from origin/main -> remove
/// 8. PR merged (including squash-merged) -> remove (#5177)
/// 9. younger than `min_age_seconds` -> keep
/// 10. fallback: `force && !safe` -> remove (`ForceOverrideUnreachable`), else keep
///
/// Step 6 is the issue-open gate (#5950), and it is the one step whose input is
/// **lazily** probed: `issue_state` is `None` for a worktree with no `issue-N`
/// branch (nothing to ask the forge about), and is otherwise called exactly
/// once, here, so the forge round-trip never happens for a worktree an earlier
/// (purely local) gate already settled. It fires when the state is anything
/// other than `"CLOSED"` — `"UNKNOWN"` (forge probe failed) included, matching
/// [`clean::classify_worktree`]'s `state != "CLOSED"` fail-closed contract and
/// this tree's own `PrLookupFailed` behavior. It is **purely subtractive on
/// removals**: it can only turn a would-be remove into a keep. The
/// `is_uncommitted || !landed` condition is what bounds it — see the module doc
/// for why "landed and clean" is deliberately still removable on an open issue
/// (partial-increment slices whose family issue never closes). Note that by
/// this point `is_uncommitted` can only still be true if `force` was passed
/// (step 5 already kept it otherwise), so the first half of that condition is
/// precisely "`--force` is about to override uncommitted work on an open
/// issue".
///
/// Step 8 is the squash-merge fix (#5177): this repo squash-merges, so a
/// merged branch's original commits are never an ancestor of `origin/main`.
/// Raw reachability (step 7) therefore cannot distinguish a safely-landed
/// squash-merged worktree from one holding genuinely unmerged work, and the
/// fallback (step 10) used to keep it forever under `UnreachableHead`. A merged
/// PR means the work IS landed regardless of reachability. Placing it AFTER
/// the uncommitted / open-PR / active-shepherd guards keeps it purely
/// **additive**: it can only turn a would-be "unreachable, keep" into a
/// remove, never override a guard that protects genuinely unmerged or
/// uncommitted work.
///
/// Step 10's `safe` guard is issue #5735: `--safe` is documented as
/// "merged-PR-only mode", and step 8 above is exactly that (a merged PR is
/// landed regardless of raw reachability). But `force` alone used to bypass
/// the *unreachable, unmerged* fallback too, silently destroying work that
/// has no merged PR at all (e.g. a closed-unmerged PR, or unpushed commits).
/// `--safe` must stay merged-PR-only by construction: when `safe` is set,
/// `force` no longer overrides this specific fallback, so `--safe --force`
/// cannot lose work that isn't backed by a merged PR.
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
    safe: bool,
    issue_state: Option<&dyn Fn() -> String>,
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

    // #5950: the issue-open gate. Probed lazily and only here, so a worktree
    // settled by any local gate above costs no forge call.
    if let Some(state) = issue_state.map(|probe| probe()) {
        let landed = head_reachable || pr_merged;
        if state != "CLOSED" && (is_uncommitted || !landed) {
            return (Decision::Keep, Reason::IssueStillOpen);
        }
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

    if force && !safe {
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
    safe: bool,
) -> (Decision, Reason) {
    let resolved_repo = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let resolved_wt = wt.path.canonicalize().unwrap_or_else(|_| wt.path.clone());
    let is_bare_or_main = resolved_wt == resolved_repo;

    let pr_lookup = wt.branch_short().map(|b| gh::has_open_pr(repo_root, &b));

    let issue_num = wt
        .branch_short()
        .and_then(|b| naming::issue_from_branch(&b));

    let is_active_shepherd = issue_num.is_some_and(|n| active_shepherds.contains(&n));

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
    let pr_merged = !head_reachable && issue_num.is_some_and(|n| pr_is_merged(repo_root, n));
    let age_seconds = worktree_age_seconds(&resolved_wt);

    // #5950: issue-open state, probed lazily by the decision tree (only when no
    // earlier, purely local gate already settled the worktree). REST, not the
    // GraphQL-backed `gh issue view` — same rationale as `pr_is_merged` above
    // and `worktree_reaper`'s own probe: GraphQL exhaustion is a live failure
    // mode here, and `--aggressive` is a bulk pass over every worktree.
    // `None` for a worktree with no `issue-N` branch — nothing to ask about.
    let issue_state_probe = issue_num.map(|n| move || gh::issue_state_rest(repo_root, n));
    let issue_state: Option<&dyn Fn() -> String> =
        issue_state_probe.as_ref().map(|f| f as &dyn Fn() -> String);

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
        safe,
        issue_state,
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
    /// #5950: worktrees kept because their issue is not `CLOSED` and the
    /// removal was not backed by landed work — a Builder may be mid-session.
    pub skipped_issue_open: usize,
    pub skipped_too_recent: usize,
    pub skipped_unreachable: usize,
    pub skipped_locked: usize,
    /// #5735: worktrees actually removed via the `ForceOverrideUnreachable`
    /// fallback — i.e. `--force` overrode the "HEAD not on origin/main —
    /// would lose work" safety skip. Counted *in addition to* `removed` (it
    /// IS a subset of the total), never folded away silently: this is the
    /// counter that lets an operator see, after the fact, that a `--force`
    /// run destroyed work with no merged PR backing it.
    pub forced_unreachable: usize,
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
    reason: Reason,
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
    // #5950: the ledger entry names `--aggressive` AND the exact decision that
    // authorized it, so a surprising removal can be traced back to the step of
    // the decision tree that produced it without re-deriving anything.
    super::removal_log::record(
        repo_root,
        "clean --aggressive",
        &wt.path,
        wt.branch_short().as_deref(),
        reason.as_str(),
    );
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
    safe: bool,
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
            decide_for_worktree(wt, repo_root, &active_shepherds, min_age_seconds, force, safe);

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
                    Reason::IssueStillOpen => {
                        stats.skipped_issue_open += 1;
                        println!(
                            "  Skip (issue is not CLOSED — a Builder may be mid-session): {label}"
                        );
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
                let is_forced_override = reason == Reason::ForceOverrideUnreachable;
                if is_forced_override {
                    // #5735: preserve the same classification text and
                    // recovery hint the ordinary `UnreachableHead` skip line
                    // prints, under a heading that marks this one as forced
                    // — a decision line must never simply vanish.
                    println!("  Force-remove (HEAD not on origin/main — would lose work): {label}");
                    if let Some(h) = &wt.head {
                        println!(
                            "    HEAD={} (recoverable via `git reflog`)",
                            &h[..h.len().min(12)]
                        );
                    }
                } else {
                    println!("  Remove ({}): {label}", reason.as_str());
                }
                match remove_aggressive_worktree(repo_root, wt, dry_run, reason) {
                    Ok(()) => {
                        stats.removed += 1;
                        if is_forced_override {
                            stats.forced_unreachable += 1;
                        }
                    }
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
    if stats.forced_unreachable > 0 {
        // #5735: a subset of `removed` above, called out separately so
        // `--force` runs never silently fold a safety override into the
        // plain total with no trace.
        println!(
            "  Forced past safety (HEAD unreachable — would lose work): {}",
            stats.forced_unreachable
        );
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
    if stats.skipped_issue_open > 0 {
        println!(
            "  Skipped (issue not CLOSED — Builder may be mid-session): {}",
            stats.skipped_issue_open
        );
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

    /// [`evaluate_aggressive_candidate`] with the #5950 issue-open probe wired
    /// to `CLOSED` — i.e. the ordinary aggressive-cleanup target, and the exact
    /// pre-#5950 behavior (the gate is a no-op for a closed issue). Every case
    /// that predates the gate goes through this so those expectations keep
    /// asserting what they always asserted.
    #[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
    fn eval_closed_issue(
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
        safe: bool,
    ) -> (Decision, Reason) {
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
            safe,
            Some(&|| "CLOSED".to_string()),
        )
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
        let (d, r) = eval_closed_issue(
            &w, false, None, false, true, true, false, true, false, None, 86400, false, false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::BareMainWorktree);
    }

    #[test]
    fn open_pr_beats_everything_else() {
        let w = wt();
        let (d, r) = eval_closed_issue(
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
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::OpenPr);
    }

    #[test]
    fn failed_pr_lookup_fails_closed() {
        let w = wt();
        let (d, r) = eval_closed_issue(
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
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::PrLookupFailed);
    }

    #[test]
    fn active_shepherd_is_kept() {
        let w = wt();
        let (d, r) = eval_closed_issue(
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
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::ActiveShepherd);
    }

    #[test]
    fn missing_sentinel_is_user_owned() {
        let w = wt();
        let (d, r) = eval_closed_issue(
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
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::UserOwned);
    }

    #[test]
    fn outside_loom_root_is_user_owned_even_with_sentinel() {
        let w = wt();
        let (d, r) = eval_closed_issue(
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
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::UserOwned);
    }

    #[test]
    fn uncommitted_changes_are_kept_unless_forced() {
        let w = wt();
        let (d, r) = eval_closed_issue(
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
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::Uncommitted);

        let (d2, _) = eval_closed_issue(
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
            false,
        );
        assert_eq!(d2, Decision::Remove);
    }

    #[test]
    fn reachable_head_is_removed_regardless_of_age() {
        let w = wt();
        let (d, r) = eval_closed_issue(
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
            false,
        );
        assert_eq!(d, Decision::Remove);
        assert_eq!(r, Reason::ReachableFromOriginMain);
    }

    #[test]
    fn unreachable_and_too_recent_is_kept() {
        let w = wt();
        let (d, r) = eval_closed_issue(
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
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::TooRecent);
    }

    #[test]
    fn unreachable_and_old_enough_is_kept_without_force() {
        let w = wt();
        let (d, r) = eval_closed_issue(
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
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::UnreachableHead);
    }

    #[test]
    fn unreachable_and_old_enough_is_removed_with_force() {
        let w = wt();
        let (d, r) = eval_closed_issue(
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
            false,
        );
        assert_eq!(d, Decision::Remove);
        assert_eq!(r, Reason::ForceOverrideUnreachable);
    }

    /// #5735: `--safe --force` must NOT lose work that has no merged PR
    /// backing it. `--safe` is documented as "merged-PR-only mode" — the
    /// unreachable-HEAD fallback (step 9) must stay a `Keep` under `safe`
    /// regardless of `force`, even though plain `force` (no `safe`) still
    /// overrides it (see `unreachable_and_old_enough_is_removed_with_force`
    /// above).
    #[test]
    fn safe_mode_keeps_unreachable_head_even_with_force() {
        let w = wt();
        let (d, r) = eval_closed_issue(
            &w,
            false,
            Some((false, true)),
            false,
            true,
            true,
            false,
            false,
            false, // PR not merged either — nothing lands this work
            Some(999_999),
            86400,
            true, // --force
            true, // --safe: must NOT override the unreachable-HEAD skip
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::UnreachableHead);
    }

    /// #5735: `--safe` narrows step 9 (the unreachable-HEAD fallback) only —
    /// it must remain purely additive everywhere else. A merged PR (step 7,
    /// the actual "merged-PR-only" removal path `--safe` is meant to allow)
    /// still removes the worktree even when `safe` is set.
    #[test]
    fn safe_mode_still_removes_when_pr_is_merged() {
        let w = wt();
        let (d, r) = eval_closed_issue(
            &w,
            false,
            Some((false, true)),
            false,
            true,
            true,
            false,
            false, // HEAD not reachable (squash-merged)
            true,  // ...but the PR is merged
            Some(999_999),
            86400,
            false, // --force not even needed
            true,  // --safe
        );
        assert_eq!(d, Decision::Remove);
        assert_eq!(r, Reason::PrMerged);
    }

    /// #5177 AC1: a squash-merged worktree has an unreachable HEAD (its commits
    /// are never an ancestor of origin/main) yet its PR is merged — it must be
    /// removed, not retained under `UnreachableHead`.
    #[test]
    fn unreachable_but_pr_merged_is_removed() {
        let w = wt();
        let (d, r) = eval_closed_issue(
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
            false,
        );
        assert_eq!(d, Decision::Remove);
        assert_eq!(r, Reason::PrMerged);
    }

    /// #5177 AC2: the merged-PR check is ADDITIVE — it must never override the
    /// uncommitted-work guard. Uncommitted changes win even when the PR merged.
    #[test]
    fn uncommitted_is_kept_even_when_pr_merged() {
        let w = wt();
        let (d, r) = eval_closed_issue(
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
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::Uncommitted);
    }

    /// #5177 AC2: an open PR still beats the merged-PR path — a worktree whose
    /// branch has an OPEN pr is kept regardless of any merged-status probe.
    #[test]
    fn open_pr_still_beats_pr_merged() {
        let w = wt();
        let (d, r) = eval_closed_issue(
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
            false,
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::OpenPr);
    }

    // --- #5950: the issue-open gate ---------------------------------------

    fn issue_state(state: &'static str) -> impl Fn() -> String {
        move || state.to_string()
    }

    /// #5950 AC: the incident's exact shape — an OPEN issue, no PR opened yet,
    /// local commits that were never pushed (so HEAD is not reachable from
    /// `origin/main`), a clean working tree, and an old-enough worktree. Before
    /// the gate this was `ForceOverrideUnreachable` (a removal) under plain
    /// `--force`; it must now be preserved, because nothing lands that work.
    #[test]
    fn open_issue_with_unpushed_commits_and_no_pr_is_kept_even_with_force() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, true)), // no OPEN pr — none has been created yet
            false,               // no claim-lock: a manually run Builder session has none
            true,
            true,
            false,         // working tree itself is clean — everything is committed locally
            false,         // ...but those commits are unpushed ⇒ unreachable from origin/main
            false,         // and no merged PR lands them either
            Some(999_999), // old enough that the age gate does not save it
            86400,
            true,  // --force
            false, // no --safe
            Some(&issue_state("OPEN")),
        );
        assert_eq!(d, Decision::Keep, "an open issue's unlanded work must survive --force");
        assert_eq!(r, Reason::IssueStillOpen);
    }

    /// #5950: `--force` documents itself as overriding *uncommitted changes*.
    /// While the issue is open that override would destroy a live Builder's
    /// in-progress edits, so the gate must beat it — even when HEAD is
    /// reachable from `origin/main` (a freshly created worktree that has not
    /// committed yet, which is the state a Builder spends its first minutes in
    /// and which the age gate never even sees, since reachability is checked
    /// first).
    #[test]
    fn open_issue_with_uncommitted_work_is_kept_even_with_force() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, true)),
            false,
            true,
            true,
            true, // uncommitted edits in flight
            true, // HEAD still == origin/main (nothing committed yet)
            false,
            Some(1),
            86400,
            true, // --force would otherwise override the uncommitted guard
            false,
            Some(&issue_state("OPEN")),
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::IssueStillOpen);
    }

    /// #5950: fail closed. An `UNKNOWN` issue state (the forge probe failed) is
    /// not "CLOSED", so it must preserve — same contract as
    /// `clean::classify_worktree`'s `state != "CLOSED"` and this tree's own
    /// `PrLookupFailed`.
    #[test]
    fn unknown_issue_state_fails_closed() {
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
            false,
            Some(&issue_state("UNKNOWN")),
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::IssueStillOpen);
    }

    /// #5950: the deliberate carve-out. A partial-increment slice (`Part of
    /// #N`) merges while the family issue #N stays open indefinitely — its
    /// worktree holds nothing but landed work, so aggressive mode must still
    /// reclaim it. Without this, the gate would make `--aggressive` useless for
    /// the single largest class of vestigial worktrees in this repo.
    #[test]
    fn open_issue_with_merged_pr_and_clean_tree_is_still_removed() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, true)),
            false,
            true,
            true,
            false, // clean working tree
            false, // squash-merged ⇒ unreachable
            true,  // ...but the PR is merged: the work IS landed
            Some(999_999),
            86400,
            false, // no --force needed
            false,
            Some(&issue_state("OPEN")),
        );
        assert_eq!(d, Decision::Remove);
        assert_eq!(r, Reason::PrMerged);
    }

    /// #5950: same carve-out via the other landed-work signal — HEAD already
    /// reachable from `origin/main` with a clean tree loses nothing.
    #[test]
    fn open_issue_with_reachable_head_and_clean_tree_is_still_removed() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, true)),
            false,
            true,
            true,
            false, // clean working tree
            true,  // HEAD is on origin/main
            false,
            Some(999_999),
            86400,
            false,
            false,
            Some(&issue_state("OPEN")),
        );
        assert_eq!(d, Decision::Remove);
        assert_eq!(r, Reason::ReachableFromOriginMain);
    }

    /// #5950: worktrees with no `issue-N` branch (detached, `pr-NNNN`, arbitrary
    /// user paths) have no issue state to consult — `None` — and the tree must
    /// behave exactly as it did before the gate existed.
    #[test]
    fn no_issue_number_leaves_the_decision_tree_unchanged() {
        let mut w = wt();
        w.branch = None;
        w.detached = true;
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            None, // branchless ⇒ no PR lookup either
            false,
            true,
            true,
            false,
            false,
            false,
            Some(999_999),
            86400,
            true,
            false,
            None,
        );
        assert_eq!(d, Decision::Remove);
        assert_eq!(r, Reason::ForceOverrideUnreachable);
    }

    /// #5950: the gate is *purely subtractive on removals* — it must never turn
    /// a pre-existing `Keep` into a `Remove`, nor change which guard reports a
    /// keep that an earlier (cheaper, purely local) gate already made. An open
    /// issue whose worktree is user-owned still reports `UserOwned`.
    #[test]
    fn earlier_local_gates_still_win_over_the_issue_gate() {
        let w = wt();
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((false, true)),
            false,
            true,
            false, // no .loom-managed sentinel
            false,
            false,
            false,
            Some(999_999),
            86400,
            true,
            false,
            Some(&issue_state("OPEN")),
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::UserOwned);
    }

    /// #5950: the probe is lazy — a worktree settled by a purely local gate
    /// must cost no forge round-trip at all.
    #[test]
    fn issue_state_is_not_probed_when_a_local_gate_settles_it() {
        let w = wt();
        let probed = std::cell::Cell::new(0_u32);
        let probe = || {
            probed.set(probed.get() + 1);
            "OPEN".to_string()
        };
        let (d, r) = evaluate_aggressive_candidate(
            &w,
            false,
            Some((true, true)), // open PR settles it immediately
            false,
            true,
            true,
            false,
            false,
            false,
            None,
            86400,
            true,
            false,
            Some(&probe),
        );
        assert_eq!(d, Decision::Keep);
        assert_eq!(r, Reason::OpenPr);
        assert_eq!(probed.get(), 0, "the forge must not be probed for an already-settled worktree");
    }

    #[test]
    fn enumerate_git_worktrees_returns_empty_on_non_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(enumerate_git_worktrees(dir.path()).is_empty());
    }

    // --- end-to-end `clean_aggressive` regression coverage (#5735) --------

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed in {}", dir.display());
    }

    /// Build a repo with a real `origin/main` remote-tracking ref (via a bare
    /// "origin" and a push) plus one `.loom-managed`, `.loom/worktrees/`-nested
    /// worktree whose HEAD is a commit made *after* the push — i.e. genuinely
    /// unreachable from `origin/main`, exactly the "closed-unmerged PR" /
    /// "unpushed commits" shape from the issue's repro. The worktree is
    /// left detached (no branch) so the decision tree never needs a `gh`
    /// call (`pr_lookup` short-circuits to `None` for a branchless worktree).
    fn repo_with_unreachable_worktree() -> (tempfile::TempDir, PathBuf) {
        let origin_dir = tempfile::tempdir().unwrap();
        git(origin_dir.path(), &["init", "-q", "--bare"]);

        let repo_dir = tempfile::tempdir().unwrap();
        git(repo_dir.path(), &["init", "-q", "--initial-branch=main"]);
        git(repo_dir.path(), &["config", "user.email", "loom@example.com"]);
        git(repo_dir.path(), &["config", "user.name", "Loom Test"]);
        git(repo_dir.path(), &["commit", "-q", "--allow-empty", "-m", "seed"]);
        git(
            repo_dir.path(),
            &[
                "remote",
                "add",
                "origin",
                origin_dir.path().to_str().unwrap(),
            ],
        );
        git(repo_dir.path(), &["push", "-q", "origin", "main"]);

        // A commit that lands ONLY in the worktree, never pushed — unreachable
        // from origin/main by construction.
        let wt_path = repo_dir
            .path()
            .join(".loom")
            .join("worktrees")
            .join("pr-9999");
        git(
            repo_dir.path(),
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                wt_path.to_str().unwrap(),
                "main",
            ],
        );
        git(&wt_path, &["commit", "-q", "--allow-empty", "-m", "unpushed work"]);
        std::fs::write(wt_path.join(LOOM_MANAGED_SENTINEL), "").unwrap();

        (repo_dir, wt_path)
    }

    /// #5735 AC: `--safe --force --dry-run` must not lose a worktree whose
    /// HEAD is unreachable from `origin/main` and has no merged PR — it must
    /// stay classified `Skip (HEAD not on origin/main — would lose work)`,
    /// not get folded into the removal total.
    #[test]
    fn safe_force_dry_run_keeps_unreachable_worktree() {
        let (repo_dir, _wt_path) = repo_with_unreachable_worktree();

        let stats = clean_aggressive(
            repo_dir.path(),
            /* dry_run */ true,
            /* force */ true,
            /* safe */ true,
            0,
        );

        assert_eq!(stats.removed, 0, "safe mode must not remove the unreachable worktree");
        assert_eq!(
            stats.forced_unreachable, 0,
            "nothing was forced past the safety skip under --safe"
        );
        assert_eq!(
            stats.skipped_unreachable, 1,
            "the unreachable worktree must still be counted as skipped"
        );
    }

    /// Contrast case: plain `--force` (no `--safe`) still overrides the skip
    /// (documented, pre-existing behavior) — but the override must be
    /// reported under the distinct `forced_unreachable` counter, not folded
    /// silently into `removed`.
    #[test]
    fn force_without_safe_removes_but_counts_it_as_forced() {
        let (repo_dir, _wt_path) = repo_with_unreachable_worktree();

        let stats = clean_aggressive(
            repo_dir.path(),
            /* dry_run */ true,
            /* force */ true,
            /* safe */ false,
            0,
        );

        assert_eq!(stats.removed, 1, "plain --force still overrides the unreachable-HEAD skip");
        assert_eq!(
            stats.forced_unreachable, 1,
            "the override must be visible via a distinct counter, not folded into `removed`"
        );
        assert_eq!(stats.skipped_unreachable, 0);
    }
}
