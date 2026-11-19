//! Shared worktree command names and adapters for both CLI binaries (#14474).

use anyhow::Result;
use clap::Subcommand;

use crate::worktree_cli::pr_list::PrList;

/// The worktree commands used by the create, reuse and remove paths.
#[derive(Subcommand)]
pub enum WorktreeCommands {
    /// `worktree.sh`'s repo-global worktree-add lock (#8195, slice 1) — the
    /// lock every destructive path in that script stands behind, and which
    /// #6014/#6017 showed could be released by a holder that no longer owned
    /// it.
    #[command(subcommand)]
    WorktreeLock(super::worktree_lock::WorktreeLockCommand),

    /// `worktree.sh remove <N>` (#8195, slice 3): the operator-facing
    /// single-worktree removal verb, and the destructive half of that script —
    /// `git worktree remove --force`, the #5177 `rm -rf` fallback, the #7239
    /// cargo-target-dir reclaim and the squash-aware `git branch -D` all hang
    /// off it. Exit 0 = removed or an idempotent no-op, 1 = refused (nothing
    /// deleted) or the removal failed.
    WorktreeRemove(super::worktree_remove::WorktreeRemoveArgs),

    /// `worktree.sh`'s post-`git worktree add` symlink provisioning (#8195,
    /// slice 4): root and nested `node_modules`, `worktree.linkPaths`,
    /// `.mcp.json`, and the `info/exclude` entry each one needs so `git add
    /// -A` cannot stage it (#3528/#5474). The part of the create path that is
    /// all path interpolation — four `ln -s "$src" "$dst"` pairs and a
    /// `find | read` loop — which is #7858's class. Exit 0 always: this is
    /// best-effort by contract and the worktree already exists.
    /// `--retire-aliases` instead unlinks the pnpm `node_modules` aliases
    /// pre-#8944 worktrees still carry (#9152; exit 1 if an unlink failed).
    WorktreeLink(super::worktree_link::WorktreeLinkArgs),

    /// `worktree.sh`'s crash-debris pre-flight (#8195, slice 5): the stale
    /// `index.lock`/`HEAD.lock`/`gitdir.lock` sweep and the **orphan guard**
    /// that `rm -rf`s an `issue-<N>` dir `git worktree list` does not know
    /// about. The guard whose false answer deleted a LIVE worktree twice over
    /// (#7858/#7849 — a porcelain path split on whitespace, and a candidate
    /// resolved logically instead of physically). Exit 0 always: both shell
    /// call sites already discard the status with `|| true`.
    WorktreeCleanup(super::worktree_cleanup::WorktreeCleanupArgs),

    /// `worktree.sh`'s `_handle_feature_branch_in_main_worktree` (#8195 slice
    /// 7): the recovery `_try_worktree_add` falls into when `git worktree
    /// add` refuses because the target branch is already checked out in the
    /// main workspace. The one arm of the create path that is pure string
    /// parsing of an arbitrary git error message — #7858's class again. Exit
    /// 0 = handled (no retry), 1 = not this error, 2 = auto-recovered, retry.
    WorktreeBranchConflict(super::worktree_branch_conflict::WorktreeBranchConflictArgs),

    /// `worktree.sh`'s post-`git worktree add` submodule initialization
    /// (#8195 slice 8): the `git submodule status | grep '^-' | awk '{print
    /// $2}'` work list — #7858's whitespace-split-path class, on the string
    /// that is then used as BOTH a `--reference` directory and a git
    /// pathspec — plus a `timeout(1)` that does not exist on a stock macOS, a
    /// `--reference` fast path that never once fired, and a `$$`-keyed
    /// failure flag in world-writable `/tmp`. Exit 0 always: best-effort by
    /// contract, and the worktree already exists.
    WorktreeSubmodules(super::worktree_submodules::WorktreeSubmodulesArgs),

    /// `worktree.sh`'s CLOSED-UNMERGED arm (#9083): the third state a pushed
    /// `origin/feature/issue-N` can be in, next to the open-PR (#4823/#7765)
    /// and merged-PR (#5657) arms that already had answers. A tip that is the
    /// head of a PR closed WITHOUT merging carries work somebody decided not
    /// to take, and reusing it silently seeded a worktree with a reverted
    /// slice plus a rejected follow-up, tens of commits behind `main` (the
    /// #8195 incident). Exit 0 = proceed and reuse as before, 1 = refuse; every
    /// inability to decide is 0, because a forge outage must never block
    /// worktree creation.
    WorktreeClosedPrBranch(super::worktree_closed_pr_branch::WorktreeClosedPrBranchArgs),

    /// `worktree.sh`'s `--sparse <paths...>` / `--full` family (#8195 slice
    /// 10), on both arms it reaches: after `git worktree add --no-checkout`,
    /// and the "worktree already exists" re-configure early exit. Retires a
    /// silent exit 128 on any cone git rejects, an unescaped cone-to-JSON
    /// builder, and a `git worktree list | grep -q` substring registration
    /// check that could disagree with the orphan guard about the same
    /// directory. Exit 0 applied, 1 refused/failed, 2 could not run.
    WorktreeSparse(super::worktree_sparse::WorktreeSparseArgs),

    /// `worktree.sh`'s base-ref preparation (#8195 slice 13): the fetch of
    /// `origin/$DEFAULT_BRANCH` and the `--base <branch>` stacked-PR
    /// resolution (validate, fetch, prefer `origin/<base>`, fall back to a
    /// local branch, refuse when neither exists). Prints `TOKEN<TAB>text`
    /// records the shell replays; `--json` refusal documents are built with
    /// `serde_json` instead of spliced by hand. Exit 0 resolved, 1 refused.
    WorktreeBase(super::worktree_base::WorktreeBaseArgs),

    /// `worktree.sh`'s in-worktree predicate and both decisions it gated
    /// (#8195 slice 11): the `--check` verb, and the create path's
    /// auto-navigation out of a worktree. The retired
    /// `[[ "$(git rev-parse --git-common-dir)" != "$(git rev-parse
    /// --show-toplevel)/.git" ]]` compared a path git answers RELATIVELY
    /// against an absolute one, so it was constant-true from every position a
    /// caller can stand in: `--check` reported the primary clone as a worktree
    /// and its exit 1 was dead code, and every `worktree.sh <N>` printed four
    /// spurious navigation lines. Exit 0/1 are the verb's two answers;
    /// `--porcelain` (the create arm's record stream) is always 0.
    WorktreeCheck(super::worktree_check::WorktreeCheckArgs),

    /// `worktree.sh`'s "the worktree directory already exists" arm, whole
    /// (#8195 slice 12): the registration probe, the preserve-vs-reset verdict,
    /// the #3548 sentinel back-fill and the stale-worktree `git reset --hard`
    /// behind its #6334 rescue guard. Retires the LAST unanchored
    /// `git worktree list | grep -q "$WORKTREE_PATH"` in the script — a
    /// substring match against symlink-RESOLVED paths, which refused a live
    /// worktree on any repo reached through a symlink (every macOS `/tmp`
    /// fixture) and accepted an unregistered `issue-4` beside a registered
    /// `issue-44`, then wrote a sentinel into it. Exit 0 = the worktree is
    /// usable, 1 = not a registered worktree (or the sentinel could not be
    /// written).
    WorktreeExisting(super::worktree_existing::WorktreeExistingArgs),

    /// `worktree.sh`'s LOCAL-branch reuse arm, whole (#8195 slice 14): the
    /// "already exists - reusing it" warning, the #6095/#6100 upstream
    /// correction (reached in-process, retiring the shell's last
    /// `_worktree_upstream_check` call site), the #8280 already-landed refusal
    /// and its degenerate-tip guard, and the base-ref divergence warning. Four
    /// steps whose ORDER is the contract, so they move as one unit. Retires a
    /// hand-spliced `--json` refusal document that was not valid JSON for any
    /// refname holding a quote — `git check-ref-format` permits one, and
    /// `$BRANCH_NAME` is operator input via the custom-branch argument. Exit 0
    /// = reuse, 1 = refuse; every inability to decide is 0, because a forge
    /// outage must never block worktree creation.
    WorktreeBranchReuse(super::worktree_branch_reuse::WorktreeBranchReuseArgs),

    /// The forge round-trip behind `lib/worktree-forge-pr-check.sh`'s #7765
    /// fresh-branch-shadow guard (#8195 slice 15) — the ORIGIN-branch sibling
    /// of `WorktreeBranchReuse`'s LOCAL-branch arm. Answers whether an OPEN
    /// PR already head-matches a branch `worktree.sh` is about to create
    /// fresh, distinguishing "confirmed no PR" / "no forge remote at all,
    /// nothing to shadow" from "the query itself failed" — the #7863
    /// regression was exactly those last two collapsing into each other.
    /// Prints a `TOKEN<TAB>text` record stream; always exits 0 (a query, not
    /// a refusal — the shell library still builds the `jq -cn` refusal
    /// documents itself).
    WorktreeOpenPr(super::worktree_open_pr::WorktreeOpenPrArgs),
}

impl WorktreeCommands {
    pub(crate) fn run(self) -> Result<()> {
        self.run_with(PrList::Legacy)
    }

    /// Run the same command with tools-mode routing for nested forge reads.
    pub fn run_tools(self) -> Result<()> {
        self.run_with(PrList::Tools)
    }

    fn run_with(self, pr_list: PrList) -> Result<()> {
        match self {
            Self::WorktreeLock(args) => args.run(),
            Self::WorktreeRemove(args) => args.run(pr_list),
            Self::WorktreeLink(args) => args.run(),
            Self::WorktreeCleanup(args) => args.run(),
            Self::WorktreeBranchConflict(args) => args.run(),
            Self::WorktreeSubmodules(args) => args.run(),
            Self::WorktreeClosedPrBranch(args) => args.run(pr_list),
            Self::WorktreeSparse(args) => args.run(),
            Self::WorktreeBase(args) => args.run(),
            Self::WorktreeCheck(args) => args.run(),
            Self::WorktreeExisting(args) => args.run(pr_list),
            Self::WorktreeBranchReuse(args) => args.run(pr_list),
            Self::WorktreeOpenPr(args) => args.run(pr_list),
        }
    }
}
