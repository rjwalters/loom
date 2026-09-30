//! Which worktree paths a merged PR owns, for post-merge cleanup (#8191 slice
//! of `merge-pr.sh`'s worktree-cleanup block; `merge-pr.sh` lines ~3019-3033
//! before this port).
//!
//! # What it decides
//!
//! Once a merge is confirmed and `--no-cleanup-worktree` / `LOOM_PRESERVE_WORKTREE`
//! / `--worktree-path` have all been ruled out, `merge-pr.sh` has to name the
//! worktrees this PR could have left behind. Three answers come out of one
//! branch-name classification:
//!
//! - **the issue number**, if any. `^feature/issue-([0-9]+)$` and nothing
//!   looser: the strict anchored form is what keeps `release-1` and
//!   `fix-bug-42` classifying as PR-style rather than issue-style, so a
//!   trailing-number heuristic cannot point cleanup at `issue-1` / `issue-42`.
//!   Downstream it is also the input to the #4186 close-target gate, which
//!   only runs when an issue number exists.
//! - **the default cleanup target**: `<root>/issue-<N>` for a Loom issue
//!   branch (the Builder's worktree), `<root>/pr-<PR>` otherwise (an
//!   external-fork or ad-hoc branch, #3358 — the only worktree a Judge/Doctor
//!   could have made for it).
//! - **the co-existing Judge/Doctor review worktree**, `<root>/pr-<PR>`, but
//!   ONLY on the issue-branch side (#6264). `pr-worktree.sh` creates that path
//!   for an ordinary `feature/issue-<N>` PR whenever no builder worktree
//!   existed at review time, and it can then co-exist with one created later;
//!   before #6264 only the external-fork branch ever considered a `pr-<N>`
//!   path, so a Judge review worktree on an ordinary issue branch survived
//!   every merge. On the non-issue side it stays `None` rather than repeating
//!   `default_path` — the shell left `JUDGE_PR_WT_PATH` empty there precisely
//!   so the second call site is not a duplicate of the first.
//!
//! `<root>` is the override-aware worktree base
//! ([`crate::worktree_root::worktree_root_readable`]), which is why this port
//! also retires `merge-pr.sh`'s `source lib/worktree-root.sh`: the resolution
//! precedence (env > `worktree.root` > `<repo>/.loom/worktrees`), the
//! repo-basename namespacing, the relative-override rejection and the
//! unreadable-target fallback now come from the one Rust implementation the
//! daemon already had, instead of a second copy reached through a sourced lib.
//!
//! # What stays in the shell
//!
//! Every filesystem question. `plan` never touches disk beyond the worktree-root
//! resolution the bash helper also did (`ls` on an overridden base): whether
//! either path EXISTS, whether it carries a `.loom-managed` sentinel, and the
//! remove-vs-preserve decision are the shell's `[[ -d ]]` tests and the
//! already-ported `merge-pr worktree-preserve` / `dirty-guard` verbs. Naming a
//! path is not deciding to remove it.
//!
//! # Fail direction: OPEN, and the shell must gate BOTH removal call sites
//!
//! A missing or older daemon leaves `merge-pr.sh` with no target names, and the
//! shell turns that into "remove nothing" by gating the entire removal path on a
//! non-empty `$DEFAULT_WT_PATH` — the convention call site AND the porcelain
//! discovery fallback. Gating only the first is not enough, and this is the
//! subtle part worth stating once: an empty `$DEFAULT_WT_PATH` fails `[[ -d ]]`
//! and so falls INTO discovery, which finds this very worktree by branch (a Loom
//! builder worktree at `issue-<N>` tracks `feature/issue-<N>` and carries
//! `.loom-managed`) while `$ISSUE_NUM` is also empty — so
//! `_worktree_cleanup_decide` omits `--preserve-check` and #4186's
//! still-open-issue protection is skipped. Ungated, a degraded daemon would
//! therefore DELETE a worktree the healthy one preserves: a fail-direction
//! inversion in the #5031 data-loss class, not merely a missed cleanup.
//!
//! With both gated, the merge (already complete by this point) is unaffected and
//! skipped cleanup is recoverable by `loom-clean`, the daemon's reaper, or the
//! next merge; removing the wrong path is not. Every downstream guard on this
//! path already requires the same binary, so a daemon that cannot answer here
//! would have declined the removal anyway.

use std::path::{Path, PathBuf};

/// The `feature/issue-<N>` prefix the strict classification requires.
const ISSUE_BRANCH_PREFIX: &str = "feature/issue-";

/// The three names post-merge cleanup works from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The issue number as WRITTEN in the branch name, not a parsed integer —
    /// the shell interpolated bash's `${BASH_REMATCH[1]}` straight into the
    /// path, so `feature/issue-007` names `issue-007`, not `issue-7`.
    pub issue_num: Option<String>,
    /// The convention path this merge's own worktree would be at.
    pub default_path: PathBuf,
    /// A co-existing Judge/Doctor review worktree (#6264), issue branches only.
    pub judge_pr_path: Option<PathBuf>,
}

/// `[[ "$branch" =~ ^feature/issue-([0-9]+)$ ]]` — the captured digits, or
/// `None`.
///
/// Hand-matched rather than regex-compiled, for two reasons that are the same
/// reason: bash's `$` anchors the END OF STRING in a `[[ =~ ]]` test (no
/// multiline mode, so `feature/issue-12\n` does NOT match), and `[0-9]` inside
/// a bracket expression is an ASCII range, never a locale-collated or Unicode
/// digit class. `strip_prefix` + `is_ascii_digit` reproduces both exactly; a
/// regex crate's `\d` would not.
#[must_use]
pub fn issue_number_of(branch: &str) -> Option<&str> {
    let digits = branch.strip_prefix(ISSUE_BRANCH_PREFIX)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(digits)
}

/// Name the cleanup targets for `branch` / `pr_number` under `worktree_root`.
///
/// `pr_number` is taken as written too: `merge-pr.sh` validated it against
/// `^[0-9]+$` at parse time and then interpolated the same string, so the
/// caller's token — not a re-rendered integer — is what names `pr-<PR>`.
#[must_use]
pub fn plan(branch: &str, pr_number: &str, worktree_root: &Path) -> Plan {
    match issue_number_of(branch) {
        Some(issue) => Plan {
            issue_num: Some(issue.to_string()),
            default_path: worktree_root.join(format!("issue-{issue}")),
            judge_pr_path: Some(worktree_root.join(format!("pr-{pr_number}"))),
        },
        None => Plan {
            issue_num: None,
            default_path: worktree_root.join(format!("pr-{pr_number}")),
            judge_pr_path: None,
        },
    }
}

/// The sentinel line the shell reads: four tab-separated fields, the last two
/// possibly empty.
///
/// ```text
/// LOOM-CLEANUP-PATHS<TAB><issue-num><TAB><default-path><TAB><judge-pr-path>
/// ```
///
/// A tab or newline inside any field would make that framing ambiguous, and an
/// ambiguous answer here names a path something later force-removes. Only the
/// worktree root can carry one (the branch already passed `check_branch_name`,
/// #9106, and the PR number is digits), so this returns `None` instead —
/// rendered by the CLI as a non-zero exit, which the shell's fail-open wrapper
/// turns into "no targets, clean up nothing".
#[must_use]
pub fn render(plan: &Plan) -> Option<String> {
    let issue = plan.issue_num.clone().unwrap_or_default();
    let default = plan.default_path.to_string_lossy().into_owned();
    let judge = plan
        .judge_pr_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    if [&issue, &default, &judge]
        .iter()
        .any(|f| f.contains('\t') || f.contains('\n'))
    {
        return None;
    }
    Some(format!("LOOM-CLEANUP-PATHS\t{issue}\t{default}\t{judge}\n"))
}

#[cfg(test)]
mod tests;
