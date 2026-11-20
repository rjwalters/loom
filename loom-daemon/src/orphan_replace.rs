//! Replacement PRs for an orphaned stacked child in a merge-mode repository
//! (#12767, part of the conflict-stacking tracker #12757).
//!
//! # Why a replacement
//!
//! #12760 flags (`loom:merge-conflict` + `<!-- loom:orphan-stack flagged … -->`)
//! a stacked child whose parent PR closed unmerged, but in a merge-mode
//! repository it neither rewrites nor retargets it: a retarget alone would
//! carry the closed parent's commits into the child's PR and from there into
//! the default branch, and the rewrite that avoids that is a force-push that
//! #10771 forbids. The operator ruling on #12767 picks a third path: open a
//! **replacement PR** from the default branch that carries only the child's own
//! commits, link it both ways to the original, and close the original. The
//! original branch is never rewritten, and no merge ever carries the rejected
//! parent's ancestry into the default branch.
//!
//! # Who does what
//!
//! Doctor runs the recipe (`defaults/roles/doctor.md`, "Orphan-stack
//! replacement"); it reads the forge and writes the PRs, comments and the
//! close. This module is the part that must not be a judgment call:
//!
//! - [`verify`]: the child-only commit range, with the boundary checks. The
//!   closed parent's head must be readable and an **ancestor** of the child's
//!   head, the range must be non-empty, and it must hold no merge commit.
//!   Anything else is a [`Refusal`], never a guessed replay.
//! - [`create`]: replay that range onto the freshly fetched default-branch tip
//!   in a scratch worktree and push it as a NEW branch ([`replacement_branch`]).
//!   The push is not forced. A conflict is a [`Refusal::Conflict`]: the
//!   replacement depends on something the default branch does not have (very
//!   likely the rejected parent), which is Doctor's investigation to make.
//! - [`close_decision`]: the original is closed only once the replacement
//!   exists and is linked, and never while open PRs are stacked on it.
//! - [`replaced_marker`] / [`replaced_by`]: the retry marker. The branch name is
//!   a function of the child PR, so a retry finds the same branch and the PR
//!   open for it; the marker records the PR number once it exists.
//!
//! The replacement carries no label and no verdict from the original: it enters
//! the queue as a fresh `loom:review-requested` PR and is reviewed and CI-tested
//! from scratch.

use std::path::Path;

use crate::claim_reconciliation::orphan_stack_git as git;
use crate::merge_pr::sequence::is_full_sha;

/// Prefix of the marker Doctor puts on the comment that links the original to
/// its replacement. The full line is `<!-- loom:orphan-stack replaced by=<N> -->`.
pub const REPLACED_MARKER_PREFIX: &str = "<!-- loom:orphan-stack replaced";

/// The marker line recording that PR `by` replaces the original.
#[must_use]
pub fn replaced_marker(by: u32) -> String {
    format!("{REPLACED_MARKER_PREFIX} by={by} -->")
}

/// The replacement PR number named by the newest `replaced` marker in
/// `bodies` (oldest first), if any.
///
/// The caller passes only comments from trusted authors
/// (`.loom/docs/comment-trust.md`): a marker from anyone else is prose.
#[must_use]
pub fn replaced_by(bodies: &[String]) -> Option<u32> {
    bodies.iter().rev().find_map(|b| {
        let rest = &b[b.find(REPLACED_MARKER_PREFIX)? + REPLACED_MARKER_PREFIX.len()..];
        let rest = rest.trim_start().strip_prefix("by=")?;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok().filter(|n| *n > 0)
    })
}

/// The branch the replacement for child PR `child_pr` is pushed to. Derived
/// from the PR number alone, so a retry targets the same branch and can never
/// open a second replacement. Not a stackable Loom branch name, so the
/// replacement is never itself taken for a stack parent.
#[must_use]
pub fn replacement_branch(child_pr: u32) -> String {
    format!("orphan-replace/pr-{child_pr}")
}

/// Everything [`verify`] and [`create`] need about the orphan.
#[derive(Debug, Clone)]
pub struct Orphan<'a> {
    /// The closed-unmerged parent PR and its last head.
    pub parent_pr: u32,
    pub parent_head: &'a str,
    /// The flagged child PR and its current head.
    pub child_pr: u32,
    pub child_head: &'a str,
}

/// Why no replacement was produced. Every variant means nothing was pushed and
/// no PR should be opened; Doctor comments the reason on the original and
/// leaves the flag in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A sha argument is not a full 40-hex commit id.
    BadSha(String),
    /// The closed parent's head, or the child's, is not readable even after
    /// fetching `refs/pull/<n>/head`.
    MissingHead(String),
    /// The parent head is not an ancestor of the child head: the parent was
    /// rewritten after the child branched, so `parent..child` would include
    /// the parent's old commits.
    ParentNotAncestor,
    /// `parent..child` is empty: the child has nothing of its own.
    EmptyRange,
    /// The range holds a merge commit; a linear replay cannot reproduce it.
    MergeInRange,
    /// The replay stopped on conflicts in these paths: the child's change
    /// depends on something the default branch lacks.
    Conflict(Vec<String>),
    /// The remote already has the replacement branch at another commit.
    BranchTaken(String),
}

/// A failure that is not a verdict on the orphan: nothing was pushed and a
/// retry may succeed (a fetch, a scratch worktree, a push).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transient(pub String);

/// The result of a step that can refuse or fail transiently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Refused(Refusal),
    Transient(Transient),
}

impl From<Refusal> for Failure {
    fn from(r: Refusal) -> Self {
        Self::Refused(r)
    }
}

impl Refusal {
    /// A stable token for logs and the Doctor recipe.
    #[must_use]
    pub fn token(&self) -> &'static str {
        match self {
            Self::BadSha(_) => "BAD_SHA",
            Self::MissingHead(_) => "MISSING_HEAD",
            Self::ParentNotAncestor => "PARENT_NOT_ANCESTOR",
            Self::EmptyRange => "EMPTY_RANGE",
            Self::MergeInRange => "MERGE_IN_RANGE",
            Self::Conflict(_) => "CONFLICT",
            Self::BranchTaken(_) => "BRANCH_TAKEN",
        }
    }

    /// One line for the comment Doctor posts on the original PR.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::BadSha(s) => format!("`{s}` is not a full commit id"),
            Self::MissingHead(w) => format!("{w} is not readable, even from `refs/pull/<n>/head`"),
            Self::ParentNotAncestor => {
                "the closed parent's head is not an ancestor of this PR's head: the parent was \
                 rewritten after this branch was cut, so its old commits cannot be told apart \
                 from this PR's own"
                    .to_string()
            }
            Self::EmptyRange => {
                "this PR has no commits of its own beyond the closed parent".to_string()
            }
            Self::MergeInRange => {
                "this PR's own range contains a merge commit, which a linear replay cannot \
                 reproduce"
                    .to_string()
            }
            Self::Conflict(paths) => format!(
                "replaying this PR's own commits onto the default branch conflicts in {}; the \
                 change very likely depends on the closed parent",
                paths
                    .iter()
                    .map(|p| format!("`{p}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::BranchTaken(why) => why.clone(),
        }
    }
}

/// The child-only commit range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boundary {
    /// `parent_head..child_head`, oldest first.
    pub commits: Vec<String>,
}

fn head_readable(repo: &Path, pr: u32, sha: &str, what: &str) -> Result<(), Failure> {
    match git::ensure_pr_head(repo, pr, sha) {
        git::HeadFetch::Present => Ok(()),
        git::HeadFetch::FetchFailed(e) => {
            Err(Failure::Transient(Transient(format!("{what} did not fetch: {e}"))))
        }
        git::HeadFetch::Missing => Err(Refusal::MissingHead(format!("{what} ({sha})")).into()),
    }
}

/// Establish the child-only boundary, or refuse.
///
/// # Errors
/// [`Failure::Refused`] when the boundary is not provably the child's own
/// commits; [`Failure::Transient`] when a fetch or git read failed.
pub fn verify(repo: &Path, o: &Orphan<'_>) -> Result<Boundary, Failure> {
    for sha in [o.parent_head, o.child_head] {
        if !is_full_sha(sha) {
            return Err(Refusal::BadSha(sha.to_string()).into());
        }
    }
    head_readable(
        repo,
        o.parent_pr,
        o.parent_head,
        &format!("the closed parent #{}'s head", o.parent_pr),
    )?;
    head_readable(repo, o.child_pr, o.child_head, &format!("PR #{}'s head", o.child_pr))?;
    match git::is_ancestor(repo, o.parent_head, o.child_head) {
        Ok(true) => {}
        Ok(false) => return Err(Refusal::ParentNotAncestor.into()),
        Err(e) => return Err(Failure::Transient(Transient(e))),
    }
    let (commits, has_merge) = git::range_commits(repo, o.parent_head, o.child_head)
        .map_err(|e| Failure::Transient(Transient(e)))?;
    if commits.is_empty() {
        return Err(Refusal::EmptyRange.into());
    }
    if has_merge {
        return Err(Refusal::MergeInRange.into());
    }
    Ok(Boundary { commits })
}

/// The pushed replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    pub branch: String,
    /// The replacement branch's head.
    pub head: String,
    /// The default-branch commit it sits on.
    pub onto: String,
    pub commits: Vec<String>,
    /// `true` when the branch already existed with exactly this range (an
    /// earlier run got this far): nothing was replayed or pushed again.
    pub reused: bool,
}

/// [`verify`], then replay the range onto `origin/<default_branch>` and push it
/// as [`replacement_branch`]. The original branch is not touched.
///
/// # Errors
/// As [`verify`], plus [`Refusal::Conflict`] and [`Refusal::BranchTaken`].
pub fn create(repo: &Path, default_branch: &str, o: &Orphan<'_>) -> Result<Replacement, Failure> {
    let boundary = verify(repo, o)?;
    let (onto, _) = crate::reconcile_stack::fetch_and_pin_target(repo, git::REMOTE, default_branch)
        .map_err(|e| Failure::Transient(Transient(e.to_string())))?;
    let branch = replacement_branch(o.child_pr);
    // A retry: the branch from an earlier run is the replacement, as long as it
    // holds exactly the child-only range. A replay would not reproduce its
    // shas (committer dates), so it is reused, never replaced.
    if let Some(tip) = git::live_tip(repo, &branch).map_err(|e| Failure::Transient(Transient(e)))? {
        return reuse_existing(repo, &branch, &tip, &onto, &boundary);
    }
    let scratch = git::scratch_path(repo, o.child_pr);
    let head = match git::replay(repo, &scratch, o.child_head, &onto, o.parent_head) {
        Ok(h) => h,
        Err(git::ReplayError::Conflict(paths)) => return Err(Refusal::Conflict(paths).into()),
        Err(git::ReplayError::Failed(e)) => return Err(Failure::Transient(Transient(e))),
    };
    git::push_new_branch(repo, &branch, &head).map_err(|e| {
        if e.starts_with("the remote already has branch") {
            Failure::Refused(Refusal::BranchTaken(e))
        } else {
            Failure::Transient(Transient(e))
        }
    })?;
    Ok(Replacement {
        branch,
        head,
        onto,
        commits: boundary.commits,
        reused: false,
    })
}

fn reuse_existing(
    repo: &Path,
    branch: &str,
    tip: &str,
    onto: &str,
    boundary: &Boundary,
) -> Result<Replacement, Failure> {
    let taken = |why: String| Failure::Refused(Refusal::BranchTaken(why));
    if let Err(e) = git::fetch_branch(repo, branch, tip) {
        return Err(Failure::Transient(Transient(e)));
    }
    let (existing, has_merge) =
        git::range_commits(repo, onto, tip).map_err(|e| Failure::Transient(Transient(e)))?;
    if has_merge || existing.len() != boundary.commits.len() {
        return Err(taken(format!(
            "the remote already has branch '{branch}' at {tip}, which is not the child-only range \
             ({} commit(s) beyond the default branch, expected {})",
            existing.len(),
            boundary.commits.len()
        )));
    }
    Ok(Replacement {
        branch: branch.to_string(),
        head: tip.to_string(),
        onto: onto.to_string(),
        commits: boundary.commits.clone(),
        reused: true,
    })
}

/// Whether the original PR may be closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseDecision {
    /// The replacement exists, both links are posted, and nothing is stacked
    /// on the original.
    Close,
    /// No replacement PR yet.
    NoReplacement,
    /// A link comment is not posted yet.
    LinksMissing,
    /// Open PRs are based on the original's branch; closing it (and so letting
    /// its branch go) would strand them. The original stays open and flagged.
    BlockedByDescendants(Vec<u32>),
}

/// The close gate. `descendants` are the open PRs whose base is the original's
/// head branch.
#[must_use]
pub fn close_decision(
    replacement: Option<u32>,
    links_posted: bool,
    descendants: &[u32],
) -> CloseDecision {
    if replacement.is_none() {
        return CloseDecision::NoReplacement;
    }
    if !links_posted {
        return CloseDecision::LinksMissing;
    }
    if !descendants.is_empty() {
        let mut d = descendants.to_vec();
        d.sort_unstable();
        d.dedup();
        return CloseDecision::BlockedByDescendants(d);
    }
    CloseDecision::Close
}

#[cfg(test)]
#[path = "orphan_replace_tests.rs"]
mod tests;
