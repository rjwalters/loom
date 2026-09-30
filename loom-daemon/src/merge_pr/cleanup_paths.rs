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
/// LOOM-CLEANUP-PATHS<TAB><default-path><TAB><issue-num><TAB><judge-pr-path>
/// ```
///
/// # Why `default-path` leads, and why that is not cosmetic
///
/// The consumer is one `IFS=$'\t' read -r DEFAULT_WT_PATH ISSUE_NUM
/// JUDGE_PR_WT_PATH`, and **tab is an IFS *whitespace* character in bash**:
/// `read` strips leading/trailing IFS whitespace from the line before splitting
/// and collapses a *run* of it into a single delimiter. So an **empty leading**
/// field cannot survive that read at all — with the fields in the order the
/// original shell assigned them (`issue` first), a non-`feature/issue-<N>`
/// branch rendered `…\t\t<default>\t`, the `\t\t` run collapsed, and
/// `$DEFAULT_WT_PATH` landed in `$ISSUE_NUM` while both real path names came
/// out empty — silently skipping post-merge cleanup for every PR-only branch
/// (`docs/…`, `security/…`, slice branches) with no warning, because the verb
/// had exited 0 with a well-formed sentinel.
///
/// Empty **trailing** fields `read` handles correctly, so putting the one field
/// that is non-empty in every case first fixes it:
///
/// ```text
/// IFS=$'\t' read -r a b c <<< $'/wt/pr-7\t\t'        → a=/wt/pr-7  b=''  c=''
/// IFS=$'\t' read -r a b c <<< $'/wt/issue-42\t42\t/wt/pr-7'
///                                                    → a=/wt/issue-42 b=42 c=/wt/pr-7
/// ```
///
/// That is only sound because `default_path` is the sole field with no `None`
/// case, and because the other two are empty *together*: [`plan`] sets
/// `issue_num` and `judge_pr_path` from the same branch classification, so
/// `issue_num.is_some() == judge_pr_path.is_some()` always (#6264's asymmetry —
/// asserted as an invariant of every differential case, and by
/// `tests::the_two_optional_fields_are_always_empty_together`). Both empties
/// therefore always move to the tail together; neither can ever become a
/// leading empty field.
///
/// A non-whitespace `IFS` delimiter was not an option: every byte except NUL
/// and newline can legally appear in a path.
///
/// A tab or newline inside any field would make the framing ambiguous, and an
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
    Some(format!("LOOM-CLEANUP-PATHS\t{default}\t{issue}\t{judge}\n"))
}

#[cfg(test)]
mod tests;
