//! Orphaned stacked children: retarget an open PR to the default branch when
//! the PR it is stacked on was closed without merging (#12760, part of the
//! conflict-stacking tracker #12757).
//!
//! # The gap
//!
//! A stacked child PR has another Loom branch (`feature/issue-<N>`) as its
//! base. When that parent **merges**, `merge-pr.sh` reconciles the children
//! (`reconcile-stack.sh`, `merge-pr retarget-children`). When the parent is
//! **closed unmerged**, nothing runs: there is no merge event, and the child is
//! left based on a branch that will never reach the default branch. Its diff
//! is measured against dead code, and if it merged as-is it would land on the
//! dead branch, not on `main`.
//!
//! # What this pass does, per tick per repo
//!
//! - **Discover.** From the shared open-PR listing: an open PR whose base is a
//!   stackable Loom branch ([`is_stackable_parent_branch`]) that is not the
//!   head of any open PR. Only those cost a forge read: the newest PR whose
//!   head is that branch. Closed with no merge ⇒ the child is an orphan. A
//!   merged parent is left to the merged-parent path; a base with no PR at all
//!   is not a Loom stack.
//! - **Gate.** Skip a child carrying a hold label (`loom:blocked`,
//!   `loom:operator`, `loom:frozen`, …), one with an agent in flight
//!   (`loom:reviewing` / `loom:treating`), and one whose issue lease is fresh.
//!   The lease, not `loom:building`: a Builder's issue keeps `loom:building`
//!   for as long as its PR is open, so a label gate would skip every
//!   Loom-built child. An unreadable lease is a skip, never "no lease".
//! - **Only Loom's own branches are rewritten** ([`head_owner`]). The child's
//!   head must be a Loom branch (the same [`is_stackable_parent_branch`]
//!   family as the base), not the default branch, and in this repository
//!   (`head.repo`). A PR whose head IS the default branch (a "sync" PR into
//!   the dead branch) is skipped before any read. A human's branch or a
//!   fork's is flagged, never pushed, so it costs no fetch on later ticks.
//! - **Rebase repos.** Replay ONLY the child's own commits (`<parent head>..
//!   <child head>`) onto the freshly fetched default-branch tip, in a scratch
//!   worktree ([`super::orphan_stack_git`]), check the result with the #9282
//!   rebase guard, publish it with a `--force-with-lease` pinned to the head
//!   the replay started from (#9487), and only then retarget the PR base to
//!   the default branch. The parent head comes from the closed PR itself and
//!   is fetched through `refs/pull/<n>/head`, so a deleted parent branch does
//!   not matter. The parent head must be an **ancestor** of the child head
//!   (`merge-base --is-ancestor`): a parent rewritten after the child branched
//!   would otherwise have its OLD commits replayed onto the default branch
//!   with the child's (`reconcile_stack`'s `ParentAncestry` refusal). Such a
//!   child is flagged, never rewritten.
//! - **Cascade.** A rewritten child may have open children of its own (based
//!   on the child's branch, which is still open). Each is replayed from the
//!   child's old head onto its new head and pushed the same way, under the
//!   same ownership and ancestry rules; its base is unchanged. A grandchild
//!   that cannot be replayed is flagged, because the discovery query will
//!   never see it again.
//! - **Merge repos** (`loom-daemon branch-update strategy` is not `rebase`,
//!   #10771). The child is NOT rewritten, and NOT retargeted either: unlike a
//!   merged parent, whose commits already reached the default branch, a
//!   closed parent's commits did not, so a retarget alone would put them into
//!   the child's PR and from there into `main`. The child is flagged instead.
//! - **Flag.** A conflict, a refusal (the closed parent's PR record has no
//!   head, or `refs/pull/<n>/head` fetched without it; a rewritten parent; a
//!   rebase-guard block), a head that is not Loom's, or merge mode gets
//!   `loom:merge-conflict` and ONE comment naming the closed parent and how to
//!   finish by hand. In rebase mode that is the exact `rebase --onto` + pinned
//!   push. In merge mode it is NOT a force-push recipe: the remedy conflicts
//!   with #10771, and the comment says so and points to #12767. The comment
//!   carries [`FLAGGED_MARKER_PREFIX`] keyed by the child's head, so the next
//!   tick does nothing until the head moves.
//!
//! # Idempotency
//!
//! A retargeted child's base is the default branch, so the discovery step no
//! longer matches it. A flagged child is skipped while its marker names its
//! current head. Nothing else is stored.
//!
//! # Fail direction
//!
//! Every unknown is a skip for this tick: a failed listing, parent lookup,
//! comment read, lease read or repository-identity read; a failed fetch
//! (including the closed parent's head); a remote head that is not the head
//! the listing reported; a rejected push. Nothing is published unless the
//! ownership, ancestry, replay, guard and lease checks all held.
//!
//! Kill switch: [`ORPHAN_STACK_ENABLED_ENV`] (default ON), nested inside the
//! master `LOOM_STALE_CLAIM_RECONCILE` switch like the sibling passes. One
//! host per workspace runs it (the role-runner shard, as the merge-sequence
//! pass does).

use std::collections::BTreeSet;
use std::path::Path;

use super::open_pr_listing::{self, RestPull};
use super::orphan_stack_git as git;
use crate::branch_update::Strategy;
use crate::merge_pr::stacked_children::is_stackable_parent_branch;
use crate::rebase_guard::Verdict as GuardVerdict;

/// Kill switch for this pass (`0`/`false`/`no`/`off` disables). Defaults ON.
pub const ORPHAN_STACK_ENABLED_ENV: &str = "LOOM_ORPHAN_STACK_RECONCILE";

/// Prefix of the marker on a flag comment. The full line is
/// `<!-- loom:orphan-stack flagged parent=<N> head=<sha> -->`.
pub const FLAGGED_MARKER_PREFIX: &str = "<!-- loom:orphan-stack flagged";

/// Prefix of the marker on the comment that reports a successful retarget.
pub const RETARGETED_MARKER_PREFIX: &str = "<!-- loom:orphan-stack retargeted";

/// The flag label (existing; see `.github/labels.yml`).
pub const MERGE_CONFLICT: &str = "loom:merge-conflict";

/// An agent is actively working the PR.
const IN_FLIGHT_LABELS: [&str; 2] = ["loom:reviewing", "loom:treating"];

/// How many stack levels above a rewritten child are replayed in one pass.
pub const MAX_CASCADE_DEPTH: usize = 8;

/// Is this pass enabled? See [`ORPHAN_STACK_ENABLED_ENV`].
#[must_use]
pub fn orphan_stack_enabled() -> bool {
    match std::env::var(ORPHAN_STACK_ENABLED_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

/// The newest closed PR whose head is the child's base branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentPr {
    pub number: u32,
    pub merged: bool,
    /// The PR's last head. `None` when the forge did not report one.
    pub head_sha: Option<String>,
}

/// What the child's base branch is, as far as stacking goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentState {
    /// The base branch is the head of an open PR: a live stack.
    Open,
    /// Its newest PR merged: the merged-parent path owns the child.
    Merged,
    /// Its newest PR closed unmerged: the child is an orphan.
    ClosedUnmerged(ParentPr),
    /// No PR has that head: not a Loom stack.
    NoPr,
}

/// The orphan predicate. `open_heads` is the head branch of every open PR;
/// `newest_closed` is the newest closed PR whose head is `base` (looked up
/// only when `base` is not an open head).
#[must_use]
pub fn parent_state(
    open_heads: &BTreeSet<&str>,
    base: &str,
    newest_closed: Option<&ParentPr>,
) -> ParentState {
    if open_heads.contains(base) {
        return ParentState::Open;
    }
    match newest_closed {
        None => ParentState::NoPr,
        Some(p) if p.merged => ParentState::Merged,
        Some(p) => ParentState::ClosedUnmerged(p.clone()),
    }
}

/// Is `pr` a discovery candidate: an open PR based on a stackable Loom branch
/// other than the default branch, and not the head of any open PR?
#[must_use]
pub fn is_candidate(pr: &RestPull, default_branch: &str, open_heads: &BTreeSet<&str>) -> bool {
    pr.state == "open"
        && pr.base_ref.as_deref().is_some_and(|b| {
            b != default_branch && is_stackable_parent_branch(b) && !open_heads.contains(b)
        })
}

/// Why a PR is left alone before any git or forge work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// A hold label: never touched.
    Held,
    /// An agent is working it right now.
    InFlight,
    /// No usable head branch or head sha.
    NoHead,
}

/// The label/shape gate. `None` = proceed.
#[must_use]
pub fn gate(pr: &RestPull) -> Option<Skip> {
    if super::VERDICT_HOLD_LABELS.iter().any(|l| pr.has_label(l)) {
        return Some(Skip::Held);
    }
    if IN_FLIGHT_LABELS.iter().any(|l| pr.has_label(l)) {
        return Some(Skip::InFlight);
    }
    let head_ok = pr
        .head_ref
        .as_deref()
        .is_some_and(|b| crate::refname::check_refname(b).is_ok());
    let sha_ok = pr
        .head_sha
        .as_deref()
        .is_some_and(crate::merge_pr::sequence::is_full_sha);
    if head_ok && sha_ok {
        None
    } else {
        Some(Skip::NoHead)
    }
}

/// Whose branch a PR's head is, for the "only rewrite Loom's own branches"
/// rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadOwner {
    /// A Loom branch in this repository: may be rewritten.
    Loom,
    /// The head IS the default branch (a sync PR into the stack branch).
    /// There is nothing of its own to replay; skipped before any read.
    DefaultBranch,
    /// A human's branch, a fork's branch, or a head whose repository the
    /// forge did not report: flagged at most, never pushed.
    Foreign,
    /// This repository's own identity could not be read: skip this tick.
    Unknown,
}

/// Classify `pr`'s head. `this_repo` is this repository's `owner/name`
/// (`None` = unreadable). A Loom head matches the same exact
/// [`is_stackable_parent_branch`] family the base is checked against, so
/// `feature/issue-12-extra` or `jdoe/fix` is not one.
#[must_use]
pub fn head_owner(pr: &RestPull, default_branch: &str, this_repo: Option<&str>) -> HeadOwner {
    let head = pr.head_ref.as_deref().unwrap_or_default();
    if head == default_branch {
        return HeadOwner::DefaultBranch;
    }
    if !is_stackable_parent_branch(head) {
        return HeadOwner::Foreign;
    }
    let Some(this) = this_repo else {
        return HeadOwner::Unknown;
    };
    if pr
        .head_repo
        .as_deref()
        .is_some_and(|r| r.eq_ignore_ascii_case(this))
    {
        HeadOwner::Loom
    } else {
        HeadOwner::Foreign
    }
}

/// The marker line for a flag on `head` caused by parent PR `parent`.
#[must_use]
pub fn flagged_marker(parent: u32, head: &str) -> String {
    format!("{FLAGGED_MARKER_PREFIX} parent={parent} head={head} -->")
}

/// Has this pass already flagged the child at `head` for `parent`?
#[must_use]
pub fn already_flagged(bodies: &[String], parent: u32, head: &str) -> bool {
    let marker = flagged_marker(parent, head);
    bodies.iter().any(|b| b.contains(&marker))
}

/// Did this pass ever flag the child (any head)?
#[must_use]
pub fn ever_flagged(bodies: &[String]) -> bool {
    bodies.iter().any(|b| b.contains(FLAGGED_MARKER_PREFIX))
}

/// Why a child was flagged instead of restacked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagReason {
    /// The replay stopped on conflicts in these paths.
    Conflict(Vec<String>),
    /// The repository does not rebase PR branches.
    MergeMode,
    /// The head branch is not a Loom branch in this repository.
    NotLoomBranch,
    /// The upstream (the parent's head) is not an ancestor of this branch's
    /// head: the parent was rewritten after this branch was cut.
    ParentRewritten,
    /// The replay could not run or its result was refused.
    Refused(String),
}

/// Everything a flag comment interpolates.
#[derive(Debug, Clone)]
pub struct FlagContext<'a> {
    /// The PR being flagged.
    pub pr: u32,
    pub branch: &'a str,
    pub head: &'a str,
    /// The PR whose history change caused the flag: the closed parent for an
    /// orphan, the rewritten child for a cascade.
    pub cause_pr: u32,
    /// `true` when `cause_pr` was closed unmerged (an orphan), `false` when it
    /// was rewritten by this pass (a cascade).
    pub orphan: bool,
    /// The `rebase --onto` arguments the operator should run.
    pub onto: &'a str,
    pub upstream: &'a str,
    /// The base to retarget to afterwards (`None` for a cascade).
    pub retarget_to: Option<&'a str>,
}

/// The flag comment body.
#[must_use]
pub fn flag_comment_body(ctx: &FlagContext<'_>, reason: &FlagReason) -> String {
    let marker = flagged_marker(ctx.cause_pr, ctx.head);
    let (heading, situation) = if ctx.orphan {
        (
            "Stacked parent closed without merging",
            format!(
                "This PR is stacked on #{} (its base branch), which was closed without \
                 merging. Its commits will never reach the default branch, so this PR must be \
                 moved onto the default branch with only its own commits.",
                ctx.cause_pr
            ),
        )
    } else {
        (
            "Stacked parent was rewritten",
            format!(
                "#{} (this PR's base) was moved onto the default branch because the PR it was \
                 stacked on closed without merging. This branch still carries the old history \
                 and must be replayed onto the new one.",
                ctx.cause_pr
            ),
        )
    };
    let why = match reason {
        FlagReason::Conflict(paths) => format!(
            "Loom tried to replay this branch's own commits and stopped on conflicts in: {}.",
            paths
                .iter()
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        FlagReason::MergeMode => format!(
            "This repository is in merge mode (`loom-daemon branch-update strategy` answered \
             `merge`). Fixing this PR means rewriting its branch: only its own commits \
             (`{upstream}..{head}`) may go onto the default branch, and the closed parent's \
             must be left out. That conflicts with this repository's policy (#10771: never \
             rebase, amend or force-push a PR branch that is already pushed), so Loom did not \
             rewrite it. A base retarget alone would carry the closed parent's commits into \
             this PR, so Loom did not retarget it either.",
            upstream = ctx.upstream,
            head = ctx.head,
        ),
        FlagReason::NotLoomBranch => format!(
            "This PR's head branch `{}` is not a Loom branch in this repository (a human's \
             branch or a fork's), and Loom never rewrites those.",
            ctx.branch
        ),
        FlagReason::ParentRewritten => format!(
            "Loom did not replay this branch: the parent head `{}` is not an ancestor of this \
             branch's head `{}`. The parent was rewritten (amended or force-pushed) after this \
             branch was cut from it, so replaying `{}..{}` would also carry the parent's old \
             commits onto the default branch.",
            ctx.upstream, ctx.head, ctx.upstream, ctx.head
        ),
        FlagReason::Refused(why) => format!("Loom did not replay this branch: {why}."),
    };
    let next = match reason {
        // #10771 forbids the rewrite the fix needs; never hand out a
        // force-push recipe as routine in a merge-mode repository.
        FlagReason::MergeMode => format!(
            "Added `{MERGE_CONFLICT}`. Loom will not fix this automatically: the merge-mode \
             remedy for a closed stacked parent is tracked in #12767. Until it is settled, a \
             human decides. One option that needs no force-push is to cherry-pick this PR's \
             own commits (`{}..{}`) onto a new branch from `{}`, open a new PR from it, and \
             close this one.\n\n",
            ctx.upstream,
            ctx.head,
            ctx.retarget_to.unwrap_or("the default branch"),
        ),
        FlagReason::NotLoomBranch => format!(
            "Added `{MERGE_CONFLICT}`. The branch's author needs to move only this PR's own \
             commits (`{}..{}`) onto the new base and update the PR, following this \
             repository's branch-update policy.\n\n",
            ctx.upstream, ctx.head
        ),
        FlagReason::Conflict(_) | FlagReason::ParentRewritten | FlagReason::Refused(_) => {
            // Only reached in a rebase-mode repository, where a pinned
            // force-push of a Loom branch is the normal update.
            let upstream = if matches!(reason, FlagReason::ParentRewritten) {
                "<the parent commit this branch was cut from>"
            } else {
                ctx.upstream
            };
            let retarget = ctx
                .retarget_to
                .map_or(String::new(), |base| format!("gh pr edit {} --base {base}\n", ctx.pr));
            format!(
                "Added `{MERGE_CONFLICT}`. To finish by hand, from a clean checkout:\n\n\
                 ```\n\
                 git fetch origin\n\
                 git checkout --detach {head}\n\
                 git rebase --onto {onto} {upstream}\n\
                 # resolve any conflicts, then:\n\
                 git push --force-with-lease=refs/heads/{branch}:{head} origin \
                 HEAD:refs/heads/{branch}\n\
                 {retarget}\
                 ```\n\n",
                head = ctx.head,
                onto = ctx.onto,
                branch = ctx.branch,
            )
        }
    };
    format!(
        "{marker}\n\
         ## {heading}\n\n\
         {situation}\n\n\
         {why}\n\n\
         {next}\
         Loom checks again once this branch's head moves.\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#12760)*"
    )
}

/// The `DELETE …/labels/<name>` path for `label` on `pr`, with the `:` in a
/// Loom label percent-encoded as every other label-removal site does.
#[must_use]
pub fn label_delete_path(pr: u32, label: &str) -> String {
    format!("repos/{{owner}}/{{repo}}/issues/{pr}/labels/{}", label.replace(':', "%3A"))
}

/// The comment posted after a successful restack + retarget.
#[must_use]
pub fn retargeted_comment_body(
    parent: u32,
    old_head: &str,
    new_head: &str,
    default_branch: &str,
    target_commit: &str,
) -> String {
    format!(
        "{RETARGETED_MARKER_PREFIX} parent={parent} old={old_head} new={new_head} -->\n\
         ## Stacked parent closed without merging — moved onto `{default_branch}`\n\n\
         #{parent}, the PR this one was stacked on, was closed without merging. Loom replayed \
         only this branch's own commits onto `{default_branch}` ({target_commit}), pushed them \
         with a lease pinned to `{old_head}`, and changed this PR's base to `{default_branch}`.\n\n\
         - Old head: `{old_head}`\n\
         - New head: `{new_head}`\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#12760)*"
    )
}

/// The forge operations this pass needs, injectable for tests.
pub trait OrphanForge {
    /// The newest closed PR whose head is `branch`. `Ok(None)` = none.
    ///
    /// # Errors
    /// The lookup could not be answered.
    fn newest_closed_pr_for_head(&self, branch: &str) -> Result<Option<ParentPr>, String>;
    /// Trusted comment bodies on PR `pr`, oldest first. `None` = unreadable.
    fn trusted_comments(&self, pr: u32) -> Option<Vec<String>>;
    /// Is issue `issue`'s lease fresh? `None` = could not tell.
    fn issue_lease_fresh(&self, issue: u32) -> Option<bool>;
    /// The repository's branch-update strategy.
    fn strategy(&self) -> Strategy;
    /// This repository's `owner/name`, compared against a PR's `head.repo`.
    /// `None` = could not tell.
    fn repository(&self) -> Option<String>;
    /// Change PR `pr`'s base to `base`.
    ///
    /// # Errors
    /// The forge refused or could not be reached.
    fn retarget(&self, pr: u32, base: &str) -> Result<(), String>;
    /// Add `label` to PR `pr`.
    ///
    /// # Errors
    /// The forge refused or could not be reached.
    fn add_label(&self, pr: u32, label: &str) -> Result<(), String>;
    /// Remove `label` from PR `pr`.
    ///
    /// # Errors
    /// The forge refused or could not be reached.
    fn remove_label(&self, pr: u32, label: &str) -> Result<(), String>;
    /// Post `body` on PR `pr`.
    ///
    /// # Errors
    /// The forge refused or could not be reached.
    fn comment(&self, pr: u32, body: &str) -> Result<(), String>;
}

/// Per-pass counts, for the log.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OrphanStats {
    /// Open PRs based on a closed-unmerged parent.
    pub orphans: usize,
    /// Orphans rebased and retargeted to the default branch.
    pub retargeted: usize,
    /// Stack levels above a restacked child that were replayed.
    pub cascaded: usize,
    /// PRs given `loom:merge-conflict` + a comment.
    pub flagged: usize,
    /// Orphans left for a later tick (a gate, an unknown, a transient failure).
    pub skipped: usize,
}

/// What one restack attempt produced.
enum Restack {
    /// Published; the new head.
    Done(String),
    /// Must be flagged.
    Flag(FlagReason),
    /// Nothing published; worth retrying on a later tick.
    Transient(String),
}

/// One pass over `open` for the repository checked out at `repo`.
pub fn reconcile_with(
    forge: &impl OrphanForge,
    repo: &Path,
    default_branch: &str,
    open: &[RestPull],
) -> OrphanStats {
    let mut stats = OrphanStats::default();
    let open_heads: BTreeSet<&str> = open
        .iter()
        .filter(|p| p.state == "open")
        .filter_map(|p| p.head_ref.as_deref())
        .collect();
    // The default-branch tip, fetched at most once per pass and only when an
    // orphan is actually being restacked.
    let mut target: Option<Result<String, String>> = None;
    // This repository's `owner/name`, read at most once per pass and only
    // when a rebase-mode orphan needs its head's owner checked.
    let mut repo_identity: Option<Option<String>> = None;

    for child in open
        .iter()
        .filter(|p| is_candidate(p, default_branch, &open_heads))
    {
        // The pure gate first: a held or in-flight child costs no forge read.
        if let Some(skip) = gate(child) {
            log::debug!(
                "claim_reconciliation (orphan stack): PR #{} (a stacked child) skipped: {skip:?}",
                child.number
            );
            continue;
        }
        // A PR from the default branch INTO the stack branch is not a stacked
        // child: it has nothing of its own to replay, and its head moves with
        // every merge, so a flag keyed on it would repeat. No read at all.
        if head_owner(child, default_branch, None) == HeadOwner::DefaultBranch {
            log::debug!(
                "claim_reconciliation (orphan stack): PR #{}'s head is '{default_branch}' — \
                 not a stacked child, skipped",
                child.number
            );
            continue;
        }
        let base = child.base_ref.as_deref().unwrap_or_default();
        let parent = match forge.newest_closed_pr_for_head(base) {
            Ok(p) => p,
            Err(e) => {
                log::debug!(
                    "claim_reconciliation (orphan stack): PR #{}: parent lookup for '{base}' \
                     failed: {e}",
                    child.number
                );
                continue;
            }
        };
        let ParentState::ClosedUnmerged(parent) = parent_state(&open_heads, base, parent.as_ref())
        else {
            continue;
        };
        stats.orphans += 1;
        let head = child.head_sha.as_deref().unwrap_or_default();
        let branch = child.head_ref.as_deref().unwrap_or_default();
        let Some(bodies) = forge.trusted_comments(child.number) else {
            stats.skipped += 1;
            continue;
        };
        if already_flagged(&bodies, parent.number, head) {
            continue;
        }
        if lease_blocks(forge, branch) {
            log::info!(
                "claim_reconciliation (orphan stack): PR #{} is orphaned by #{} but its issue \
                 lease is fresh or unreadable — left for a later tick",
                child.number,
                parent.number
            );
            stats.skipped += 1;
            continue;
        }

        let upstream = parent.head_sha.clone().unwrap_or_default();
        let upstream_shown = if upstream.is_empty() {
            "<parent-head>"
        } else {
            upstream.as_str()
        };
        let display_onto = format!("{}/{default_branch}", git::REMOTE);

        if forge.strategy() != Strategy::Rebase {
            flag(
                forge,
                &orphan_ctx(
                    child.number,
                    branch,
                    head,
                    parent.number,
                    &display_onto,
                    upstream_shown,
                    default_branch,
                ),
                &FlagReason::MergeMode,
                &mut stats,
            );
            continue;
        }
        // Only Loom's own branches in this repository are rewritten. A human
        // or fork head is flagged (once per head, like every flag), so later
        // ticks spend no fetch on it.
        let this_repo = repo_identity
            .get_or_insert_with(|| forge.repository())
            .clone();
        match head_owner(child, default_branch, this_repo.as_deref()) {
            HeadOwner::Loom => {}
            HeadOwner::Unknown | HeadOwner::DefaultBranch => {
                log::info!(
                    "claim_reconciliation (orphan stack): PR #{}: this repository's identity is \
                     unreadable, so its head's owner is unknown — left for a later tick",
                    child.number
                );
                stats.skipped += 1;
                continue;
            }
            HeadOwner::Foreign => {
                flag(
                    forge,
                    &orphan_ctx(
                        child.number,
                        branch,
                        head,
                        parent.number,
                        &display_onto,
                        upstream_shown,
                        default_branch,
                    ),
                    &FlagReason::NotLoomBranch,
                    &mut stats,
                );
                continue;
            }
        }
        let this_repo = this_repo.unwrap_or_default();
        let unreadable = if crate::merge_pr::sequence::is_full_sha(&upstream) {
            match git::ensure_pr_head(repo, parent.number, &upstream) {
                git::HeadFetch::Present => None,
                // A failed fetch is transient: skip, retry next tick. Only a
                // definite answer (no head in the record, or a fetched PR ref
                // without that commit) is flagged.
                git::HeadFetch::FetchFailed(e) => {
                    log::info!(
                        "claim_reconciliation (orphan stack): PR #{}: the closed parent #{}'s \
                         head did not fetch (retrying next tick): {e}",
                        child.number,
                        parent.number
                    );
                    stats.skipped += 1;
                    continue;
                }
                git::HeadFetch::Missing => Some(format!(
                    "`refs/pull/{}/head` fetched, but the closed parent #{}'s recorded head \
                     {upstream} is not in it",
                    parent.number, parent.number
                )),
            }
        } else {
            Some(format!("the closed parent #{}'s PR record names no head commit", parent.number))
        };
        if let Some(why) = unreadable {
            flag(
                forge,
                &orphan_ctx(
                    child.number,
                    branch,
                    head,
                    parent.number,
                    &display_onto,
                    upstream_shown,
                    default_branch,
                ),
                &FlagReason::Refused(why),
                &mut stats,
            );
            continue;
        }
        let onto = match target
            .get_or_insert_with(|| {
                crate::reconcile_stack::fetch_and_pin_target(repo, git::REMOTE, default_branch)
                    .map(|(sha, _)| sha)
                    .map_err(|e| e.to_string())
            })
            .clone()
        {
            Ok(sha) => sha,
            Err(e) => {
                log::warn!(
                    "claim_reconciliation (orphan stack): {}: could not fetch '{default_branch}' \
                     — no orphan restacked this pass: {e}",
                    repo.display()
                );
                stats.skipped += 1;
                continue;
            }
        };

        match restack(repo, child.number, branch, head, &onto, &upstream) {
            Restack::Done(new_head) => {
                if let Err(e) = forge.retarget(child.number, default_branch) {
                    // The branch is already published; the next tick finds the
                    // child still orphaned and replays the (now rebased)
                    // commits again, which is a no-op change of content.
                    log::warn!(
                        "claim_reconciliation (orphan stack): PR #{} restacked onto {onto} but \
                         the retarget to '{default_branch}' failed (retrying next tick): {e}",
                        child.number
                    );
                    stats.skipped += 1;
                    continue;
                }
                stats.retargeted += 1;
                log::info!(
                    "claim_reconciliation (orphan stack): PR #{} was stacked on #{} (closed \
                     unmerged); replayed its own commits onto {onto}, pushed {new_head} leased to \
                     {head}, retargeted to '{default_branch}' (#12760)",
                    child.number,
                    parent.number
                );
                if child.has_label(MERGE_CONFLICT) && ever_flagged(&bodies) {
                    if let Err(e) = forge.remove_label(child.number, MERGE_CONFLICT) {
                        log::warn!(
                            "claim_reconciliation (orphan stack): PR #{}: removing \
                             {MERGE_CONFLICT} failed: {e}",
                            child.number
                        );
                    }
                }
                let body =
                    retargeted_comment_body(parent.number, head, &new_head, default_branch, &onto);
                if let Err(e) = forge.comment(child.number, &body) {
                    log::warn!(
                        "claim_reconciliation (orphan stack): PR #{}: report comment failed: {e}",
                        child.number
                    );
                }
                cascade(
                    forge,
                    repo,
                    open,
                    &this_repo,
                    default_branch,
                    child,
                    head,
                    &new_head,
                    1,
                    &mut stats,
                );
            }
            Restack::Flag(reason) => flag(
                forge,
                &orphan_ctx(
                    child.number,
                    branch,
                    head,
                    parent.number,
                    &onto,
                    &upstream,
                    default_branch,
                ),
                &reason,
                &mut stats,
            ),
            Restack::Transient(e) => {
                log::info!(
                    "claim_reconciliation (orphan stack): PR #{} not restacked this tick: {e}",
                    child.number
                );
                stats.skipped += 1;
            }
        }
    }
    stats
}

/// The flag context for an orphan (as opposed to a cascade).
fn orphan_ctx<'a>(
    pr: u32,
    branch: &'a str,
    head: &'a str,
    parent: u32,
    onto: &'a str,
    upstream: &'a str,
    default_branch: &'a str,
) -> FlagContext<'a> {
    FlagContext {
        pr,
        branch,
        head,
        cause_pr: parent,
        orphan: true,
        onto,
        upstream,
        retarget_to: Some(default_branch),
    }
}

/// Does the issue behind `branch` have a fresh (or unreadable) lease?
fn lease_blocks(forge: &impl OrphanForge, branch: &str) -> bool {
    let Some(issue) =
        crate::merge_pr::reconcile::issue_from_branch(branch).and_then(|n| n.parse::<u32>().ok())
    else {
        return false;
    };
    forge.issue_lease_fresh(issue).unwrap_or(true)
}

/// Replay `upstream..head` of `branch` onto `onto`, guard it, and publish it.
///
/// `upstream` must be readable in `repo` (the caller fetched it). It must
/// also be an ancestor of `head`, or the replayed range is not "this branch's
/// own commits" — that is checked here, for the orphan and the cascade alike.
fn restack(repo: &Path, pr: u32, branch: &str, head: &str, onto: &str, upstream: &str) -> Restack {
    match git::ensure_pr_head(repo, pr, head) {
        git::HeadFetch::Present => {}
        git::HeadFetch::FetchFailed(e) => {
            return Restack::Transient(format!("its head {head} could not be fetched: {e}"))
        }
        git::HeadFetch::Missing => {
            return Restack::Transient(format!("its head {head} is not in refs/pull/{pr}/head"))
        }
    }
    match git::live_tip(repo, branch) {
        Ok(Some(live)) if live == head => {}
        Ok(Some(live)) => {
            return Restack::Transient(format!(
                "the remote branch is at {live}, not the listed head {head}"
            ))
        }
        Ok(None) => return Restack::Transient(format!("the remote has no branch '{branch}'")),
        Err(e) => return Restack::Transient(e),
    }
    // `rebase --onto <onto> <upstream>` with a non-ancestor upstream does not
    // fail: it replays every commit `upstream` cannot reach, which for a
    // parent rewritten after this branch was cut includes the parent's OLD
    // commits (reconcile_stack's `Prerequisite::ParentAncestry`). Refuse.
    match git::is_ancestor(repo, upstream, head) {
        Ok(true) => {}
        Ok(false) => return Restack::Flag(FlagReason::ParentRewritten),
        Err(e) => return Restack::Transient(e),
    }
    let scratch = git::scratch_path(repo, pr);
    let new_head = match git::replay(repo, &scratch, head, onto, upstream) {
        Ok(h) => h,
        Err(git::ReplayError::Conflict(paths)) => {
            return Restack::Flag(FlagReason::Conflict(paths))
        }
        Err(git::ReplayError::Failed(e)) => return Restack::Transient(e),
    };
    match crate::rebase_guard::assess(repo, onto, head, &new_head) {
        GuardVerdict::Clean(_) => {}
        GuardVerdict::Emptied { .. } => {
            return Restack::Flag(FlagReason::Refused(
                "replaying it would leave no change of its own (the rebase guard reported \
                 EMPTIED, #9282)"
                    .to_string(),
            ))
        }
        GuardVerdict::ResolutionDiscarded { merge, .. } => {
            return Restack::Flag(FlagReason::Refused(format!(
                "replaying it would drop the hand resolution in merge {merge} (the rebase guard \
                 reported RESOLUTION_DISCARDED, #9282)"
            )))
        }
        GuardVerdict::Unanswered(why) => log::warn!(
            "claim_reconciliation (orphan stack): PR #{pr}: rebase guard gave no answer ({why}); \
             pushing unchecked, as reconcile-stack.sh does (#9282)"
        ),
    }
    match git::push_leased(repo, branch, &new_head, head) {
        Ok(()) => Restack::Done(new_head),
        Err(e) => Restack::Transient(e),
    }
}

/// Replay every open PR stacked on `child` from `old_head` onto `new_head`.
/// `this_repo` is this repository's `owner/name`: only a Loom head in it is
/// rewritten, as for the orphan itself.
#[allow(clippy::too_many_arguments)]
fn cascade(
    forge: &impl OrphanForge,
    repo: &Path,
    open: &[RestPull],
    this_repo: &str,
    default_branch: &str,
    child: &RestPull,
    old_head: &str,
    new_head: &str,
    depth: usize,
    stats: &mut OrphanStats,
) {
    let Some(child_branch) = child.head_ref.as_deref() else {
        return;
    };
    for g in open
        .iter()
        .filter(|p| p.state == "open" && p.number != child.number)
        .filter(|p| p.base_ref.as_deref() == Some(child_branch))
    {
        let head = g.head_sha.as_deref().unwrap_or_default();
        let branch = g.head_ref.as_deref().unwrap_or_default();
        let ctx = FlagContext {
            pr: g.number,
            branch,
            head,
            cause_pr: child.number,
            orphan: false,
            onto: new_head,
            upstream: old_head,
            retarget_to: None,
        };
        let reason = if depth > MAX_CASCADE_DEPTH {
            Some(FlagReason::Refused(format!(
                "the stack above #{} is deeper than {MAX_CASCADE_DEPTH} levels",
                child.number
            )))
        } else {
            match gate(g) {
                Some(Skip::Held) => {
                    log::warn!(
                        "claim_reconciliation (orphan stack): PR #{} is stacked on rewritten #{} \
                         but carries a hold label — not touched",
                        g.number,
                        child.number
                    );
                    stats.skipped += 1;
                    continue;
                }
                Some(Skip::NoHead) => Some(FlagReason::Refused(
                    "its head branch or head commit is not usable".to_string(),
                )),
                Some(Skip::InFlight) => Some(FlagReason::Refused(
                    "an agent is working on it (loom:reviewing / loom:treating)".to_string(),
                )),
                None => match head_owner(g, default_branch, Some(this_repo)) {
                    // A sync PR from the default branch into the rewritten
                    // child: nothing of its own to replay.
                    HeadOwner::DefaultBranch => {
                        log::debug!(
                            "claim_reconciliation (orphan stack): PR #{}'s head is \
                             '{default_branch}' — not a stacked child, skipped",
                            g.number
                        );
                        continue;
                    }
                    HeadOwner::Foreign | HeadOwner::Unknown => Some(FlagReason::NotLoomBranch),
                    HeadOwner::Loom => None,
                },
            }
        };
        let reason = match reason {
            None if lease_blocks(forge, branch) => Some(FlagReason::Refused(
                "its issue lease is held, so a Builder may be pushing to it".to_string(),
            )),
            other => other,
        };
        if let Some(reason) = reason {
            flag(forge, &ctx, &reason, stats);
            continue;
        }
        match restack(repo, g.number, branch, head, new_head, old_head) {
            Restack::Done(g_new) => {
                stats.cascaded += 1;
                log::info!(
                    "claim_reconciliation (orphan stack): PR #{} (stacked on #{}) replayed onto \
                     {new_head}, pushed {g_new} leased to {head} (#12760)",
                    g.number,
                    child.number
                );
                cascade(
                    forge,
                    repo,
                    open,
                    this_repo,
                    default_branch,
                    g,
                    head,
                    &g_new,
                    depth + 1,
                    stats,
                );
            }
            Restack::Flag(reason) => flag(forge, &ctx, &reason, stats),
            // The discovery query will never see this PR again (its base is
            // open), so a transient failure here is flagged, not retried.
            Restack::Transient(e) => flag(forge, &ctx, &FlagReason::Refused(e), stats),
        }
    }
}

/// Label first (so a failed comment is retried next tick: no marker yet),
/// then the one marker-bearing comment.
fn flag(
    forge: &impl OrphanForge,
    ctx: &FlagContext<'_>,
    reason: &FlagReason,
    stats: &mut OrphanStats,
) {
    if let Err(e) = forge.add_label(ctx.pr, MERGE_CONFLICT) {
        log::warn!(
            "claim_reconciliation (orphan stack): PR #{}: adding {MERGE_CONFLICT} failed: {e}",
            ctx.pr
        );
        return;
    }
    if let Err(e) = forge.comment(ctx.pr, &flag_comment_body(ctx, reason)) {
        log::warn!("claim_reconciliation (orphan stack): PR #{}: flag comment failed: {e}", ctx.pr);
        return;
    }
    stats.flagged += 1;
    log::info!(
        "claim_reconciliation (orphan stack): flagged PR #{} ({MERGE_CONFLICT}) — {reason:?} \
         (#12760)",
        ctx.pr
    );
}

/// The live forge, through the claim-reconciliation `gh` facade.
pub struct GhOrphanForge<'a> {
    pub gh_bin: &'a Path,
    pub root: &'a Path,
}

impl GhOrphanForge<'_> {
    fn api_write(&self, op: &'static str, args: &[&str]) -> Result<(), String> {
        let out = super::gh_call::output(
            super::gh_call::write(op, self.gh_bin, self.root)
                .args(["api"])
                .args(args),
        )
        .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(super::gh_call::stderr(&out))
        }
    }
}

/// Parse a `GET pulls?head=…&state=closed` body into the newest PR whose head
/// is `branch`. GitHub ignores a `head` it cannot resolve and returns every
/// closed PR, so the rows are re-filtered on `head.ref`.
///
/// # Errors
/// Malformed JSON, or a matching row without a `merged_at` field at all.
pub fn parse_newest_closed(body: &[u8], branch: &str) -> Result<Option<ParentPr>, String> {
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(body).map_err(|e| format!("not a JSON array of PRs: {e}"))?;
    let mut best: Option<ParentPr> = None;
    for row in rows {
        if row.get("state").and_then(|s| s.as_str()) != Some("closed")
            || row.pointer("/head/ref").and_then(|s| s.as_str()) != Some(branch)
        {
            continue;
        }
        let Some(number) = row
            .get("number")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
        else {
            continue;
        };
        let merged = match row.get("merged_at") {
            Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::String(_)) => true,
            _ => return Err(format!("PR #{number} has no readable merged_at")),
        };
        let head_sha = row
            .pointer("/head/sha")
            .and_then(|s| s.as_str())
            .map(str::to_string);
        if best.as_ref().is_none_or(|b| number > b.number) {
            best = Some(ParentPr {
                number,
                merged,
                head_sha,
            });
        }
    }
    Ok(best)
}

impl OrphanForge for GhOrphanForge<'_> {
    fn newest_closed_pr_for_head(&self, branch: &str) -> Result<Option<ParentPr>, String> {
        let path = format!(
            "repos/{{owner}}/{{repo}}/pulls?head={{owner}}:{branch}&state=closed&per_page=100"
        );
        let out = super::gh_call::output(
            super::gh_call::read("orphan_stack.parent_lookup", self.gh_bin, self.root)
                .args(["api", &path]),
        )
        .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(super::gh_call::stderr(&out));
        }
        parse_newest_closed(&out.stdout, branch)
    }

    fn trusted_comments(&self, pr: u32) -> Option<Vec<String>> {
        crate::merge_pr::sequence::fetch_trusted_bodies_as(
            "orphan_stack.comments",
            &self.gh_bin.to_string_lossy(),
            self.root,
            "{owner}/{repo}",
            pr,
        )
    }

    fn issue_lease_fresh(&self, issue: u32) -> Option<bool> {
        use super::forge::LeaseProbe;
        match super::forge::fetch_freshest_lease_updated_at(self.gh_bin, self.root, issue) {
            LeaseProbe::Found(at) => Some(super::lease_is_fresh(
                at,
                chrono::Utc::now(),
                super::resolve_lease_ttl_minutes(),
            )),
            LeaseProbe::NotFound => Some(false),
            LeaseProbe::ReadFailed => None,
        }
    }

    fn strategy(&self) -> Strategy {
        let nwo = std::env::var("LOOM_REPO").ok();
        crate::branch_update::resolve(Some(self.root), nwo.as_deref(), true).strategy
    }

    fn repository(&self) -> Option<String> {
        crate::worktree_ops::gh::resolve_owner_repo(self.root).map(|(o, r)| format!("{o}/{r}"))
    }

    fn retarget(&self, pr: u32, base: &str) -> Result<(), String> {
        let path = format!("repos/{{owner}}/{{repo}}/pulls/{pr}");
        let field = format!("base={base}");
        self.api_write("orphan_stack.retarget", &["-X", "PATCH", &path, "-f", &field])
    }

    fn add_label(&self, pr: u32, label: &str) -> Result<(), String> {
        let path = format!("repos/{{owner}}/{{repo}}/issues/{pr}/labels");
        let field = format!("labels[]={label}");
        self.api_write("orphan_stack.label", &["-X", "POST", &path, "-f", &field])
    }

    fn remove_label(&self, pr: u32, label: &str) -> Result<(), String> {
        let path = label_delete_path(pr, label);
        self.api_write("orphan_stack.label", &["-X", "DELETE", &path])
    }

    fn comment(&self, pr: u32, body: &str) -> Result<(), String> {
        let path = format!("repos/{{owner}}/{{repo}}/issues/{pr}/comments");
        let field = format!("body={body}");
        self.api_write("orphan_stack.comment", &["-X", "POST", &path, "-f", &field])
    }
}

/// The daemon entry point: one pass for `root`, on the shared open-PR listing
/// when the preceding pass handed one on.
pub(super) fn reconcile(
    gh_bin: &Path,
    root: &Path,
    prefetched: Option<&[RestPull]>,
) -> OrphanStats {
    if !orphan_stack_enabled() {
        return OrphanStats::default();
    }
    // One host per workspace writes (#10634's shard, as the merge-sequence
    // pass uses): two daemons racing the same orphan would both read, and the
    // loser's lease would fail, but both would comment.
    if !crate::role_shard::decide(root).admits_role_tick() {
        return OrphanStats::default();
    }
    let Some(default_branch) = crate::worktree_cli::default_branch::resolve(root) else {
        log::debug!(
            "claim_reconciliation (orphan stack): {}: default branch unknown — pass skipped",
            root.display()
        );
        return OrphanStats::default();
    };
    let listed;
    let open = match prefetched {
        Some(rows) => rows,
        None => match open_pr_listing::list_open_prs(gh_bin, root) {
            Ok(rows) => {
                listed = rows;
                &listed[..]
            }
            Err(e) => {
                log::warn!("claim_reconciliation (orphan stack): {}: {e}", root.display());
                return OrphanStats::default();
            }
        },
    };
    let stats = reconcile_with(&GhOrphanForge { gh_bin, root }, root, &default_branch, open);
    if stats != OrphanStats::default() {
        log::info!(
            "claim_reconciliation (orphan stack): {}: {} orphan(s), {} retargeted, {} cascaded, \
             {} flagged, {} skipped (#12760)",
            root.display(),
            stats.orphans,
            stats.retargeted,
            stats.cascaded,
            stats.flagged,
            stats.skipped
        );
    }
    stats
}

#[cfg(test)]
#[path = "orphan_stack_tests.rs"]
mod tests;
