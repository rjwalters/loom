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
use super::landed::{self, Landed};
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
    /// regardless of git reachability (#5177). Since #7812 this also covers a
    /// rebase merge, proved by tree equality rather than by a merged PR.
    PrMerged,
    /// Neither the forge nor the offline tree comparison could say whether
    /// the work has landed (#7812). Fails closed: never reaped, not even
    /// under `--force` — `unknown` is not `not-landed`.
    LandedUnknown,
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
            Reason::LandedUnknown => "landed_unknown",
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
/// the injected `landed` / `has_open_pr` / `uncommitted` /
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
/// 7. landed, reachable from origin/main -> remove
/// 8. landed under rewritten SHAs (squash/rebase) -> remove (#5177/#7812)
/// 9. landed state `Unknown` -> keep (`LandedUnknown`, #7812)
/// 10. younger than `min_age_seconds` -> keep
/// 11. fallback: `force && !safe` -> remove (`ForceOverrideUnreachable`), else keep
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
/// Step 8 is the squash-merge fix (#5177), generalized in #7812: this repo
/// squash-merges, so a merged branch's original commits are never an ancestor
/// of `origin/main`. Raw reachability (step 7) therefore cannot distinguish a
/// safely-landed squash-merged worktree from one holding genuinely unmerged
/// work, and the fallback (step 11) used to keep it forever under
/// `UnreachableHead`. [`Landed::Rewritten`] — a merged PR, or tree equality
/// against `origin/main`, which also covers a rebase merge — means the work IS
/// landed regardless of reachability. Placing it AFTER the uncommitted /
/// open-PR / active-shepherd guards keeps it purely **additive**: it can only
/// turn a would-be "unreachable, keep" into a remove, never override a guard
/// that protects genuinely unmerged or uncommitted work.
///
/// Step 9 is the fail-closed arm of the same change (#7812). [`Landed::Unknown`]
/// means neither the forge nor the offline tree comparison could answer — it is
/// NOT `not-landed`, and must never fall through to step 11, where `--force`
/// would reap a worktree during a forge outage. It is the one removal-blocking
/// state `--force` cannot override.
///
/// Step 11's `safe` guard is issue #5735: `--safe` is documented as
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
    landed: Landed,
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
        if state != "CLOSED" && (is_uncommitted || !landed.is_landed()) {
            return (Decision::Keep, Reason::IssueStillOpen);
        }
    }

    match landed {
        Landed::Reachable => return (Decision::Remove, Reason::ReachableFromOriginMain),
        // #5177/#7812: work landed under rewritten SHAs (squash or rebase) is
        // landed even though its commits are never reachable from origin/main.
        // Additive to the reachability arm above.
        Landed::Rewritten => return (Decision::Remove, Reason::PrMerged),
        // #7812: fail closed. An undetermined answer must not fall through to
        // the `force && !safe` override below, which would reap on a forge
        // outage — the one outcome that loses work irrecoverably.
        Landed::Unknown => return (Decision::Keep, Reason::LandedUnknown),
        Landed::NotLanded => {}
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
    // #7812: the one shared "has this branch landed?" answer — ancestry, then
    // (only if that failed) the forge, then offline tree equality. Three-way:
    // `Unknown` reaches the decision tree as its own state, never flattened
    // into a bool here. See `worktree_ops::landed`.
    let landed_state = landed::probe(repo_root, wt.head.as_deref(), issue_num);
    let age_seconds = worktree_age_seconds(&resolved_wt);

    // #5950: issue-open state, probed lazily by the decision tree (only when no
    // earlier, purely local gate already settled the worktree). REST, not the
    // GraphQL-backed `gh issue view` — same rationale as `landed::probe`'s own
    // forge rung and `worktree_reaper`'s probe: GraphQL exhaustion is a live failure
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
        landed_state,
        age_seconds,
        min_age_seconds,
        force,
        safe,
        issue_state,
    )
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
    /// #7872: worktrees kept under `Reason::LandedUnknown` — "could not
    /// determine whether the work landed" (forge unreachable AND the tree
    /// comparison unavailable). Counted separately from
    /// `skipped_unreachable` (`Reason::UnreachableHead`, "confirmed
    /// not-landed / reachability unproven"): the two `Keep` reasons print
    /// distinct messages and must not be conflated into one summary count —
    /// "could not determine" and "confirmed unreachable" are different
    /// answers, not the same skip for two reasons.
    pub skipped_landed_unknown: usize,
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
                    Reason::LandedUnknown => {
                        stats.skipped_landed_unknown += 1;
                        println!(
                            "  Skip (could not determine whether the work landed — \
                             forge unreachable and tree comparison unavailable): {label}"
                        );
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
    if stats.skipped_landed_unknown > 0 {
        println!(
            "  Skipped (could not determine whether the work landed): {}",
            stats.skipped_landed_unknown
        );
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
mod tests;
