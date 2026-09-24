//! Stacked-PR reconciliation: plan the rebase, then execute it (#8583).
//!
//! This is the decision half of `defaults/scripts/reconcile-stack.sh`, moved
//! into Rust per `.loom/docs/shell-language-policy.md`. The script keeps its
//! name (it is `contract` — `merge-pr.sh`, role prompts and the sweep
//! lifecycle all invoke it by path) and keeps the three *forge/publish* steps
//! it always had: `gh pr view`, `push --force-with-lease`, `gh pr edit`. What
//! moved here is everything that decides **what to rebase onto, from where,
//! and in which directory** — plus the rebase itself.
//!
//! # The defect this port exists to fix
//!
//! The shell refreshed the remote default branch and then rebased onto the
//! **local** one:
//!
//! ```text
//! git fetch origin "$DEFAULT_BRANCH" 2>/dev/null || true     # updates a remote-tracking ref
//! git rebase --onto "$DEFAULT_BRANCH" "$PARENT_REF" "$CHILD" # ...uses the LOCAL branch
//! ```
//!
//! Those are two different commits whenever the local checkout has not pulled
//! since the parent squash-merged — which is the normal state at exactly the
//! moment reconciliation runs, because the merge happened on the forge, not
//! here. The failure is **silent when it is worst**: with a child whose files
//! do not overlap the parent's, the rebase reports success, the child's own
//! commits replay cleanly onto the stale base, and the just-merged parent's
//! implementation is simply *not there* any more. Nothing conflicts, nothing
//! warns, and the force-with-lease push publishes the loss. A child that
//! *does* touch the parent's files gets the visible version: a spurious
//! conflict against content that is already on the default branch.
//!
//! Three properties follow, and each is enforced below rather than documented:
//!
//! 1. **The fetch is a prerequisite, not an advisory.** `|| true` on the fetch
//!    means a network failure silently degrades to "rebase onto whatever the
//!    local branch happens to be" — the same data loss, with no fetch to blame
//!    it on. [`plan`] refuses instead.
//! 2. **The destination is a commit, not a branch name.** Once resolved it
//!    cannot drift, cannot be moved by a concurrent checkout in another
//!    worktree, and does not depend on the local branch existing at all. The
//!    local default branch is never read.
//! 3. **The branch *name* is still needed** — for `gh pr edit --base` — so the
//!    two are kept apart by type rather than by convention:
//!    [`Plan::target_commit`] mutates git, [`Plan::default_branch`] retargets
//!    the PR.
//!
//! # Fetching into an explicit refspec
//!
//! `git fetch origin main` updates `refs/remotes/origin/main` only
//! *opportunistically* — it depends on the remote having a configured fetch
//! refspec, which a bare `git remote add` in a fixture (or a repo configured
//! for a single branch) may not have. Reading that ref afterwards would then
//! return a stale value, or nothing, and the "resolved remote tip" would be a
//! guess. So the fetch names its destination explicitly:
//!
//! ```text
//! git fetch <remote> +refs/heads/<branch>:refs/remotes/<remote>/<branch>
//! ```
//!
//! After that succeeds, the ref *is* what this fetch retrieved, and it is
//! immediately resolved to a SHA. The SHA is the pin: it cannot be invalidated
//! by a later fetch, prune, or branch deletion the way a name can.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::cmd_out::{run_command, CmdOutcome};

#[cfg(test)]
mod tests;

/// Network-touching git steps (`fetch`, `ls-remote`). Generous, because a cold
/// fetch on a large repository is legitimately slow; bounded, because a hung
/// credential prompt must not park a reconciliation forever.
const GIT_NET_TIMEOUT: Duration = Duration::from_secs(600);

/// Local git steps (`rev-parse`, `merge-base`, `status`, `rebase`).
const GIT_LOCAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Which prerequisite refused, so a caller (and an operator reading the log)
/// can tell a network failure from a stale pin from a dirty tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prerequisite {
    /// The default branch could not be fetched from the remote.
    Fetch,
    /// The fetch ran but the remote has no such branch, or the fetched ref did
    /// not resolve to a commit.
    RemoteTarget,
    /// Neither the parent branch name nor its pinned ref resolves.
    ParentRef,
    /// The parent ref resolves but is not an ancestor of the child branch, so
    /// `rebase --onto` would replay the wrong commit range.
    ParentAncestry,
    /// The working tree the rebase would run in has uncommitted changes.
    DirtyWorktree,
    /// The child branch does not resolve in this repository.
    ChildBranch,
}

impl Prerequisite {
    /// A stable, greppable token naming the failing prerequisite.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Prerequisite::Fetch => "FETCH",
            Prerequisite::RemoteTarget => "REMOTE-TARGET",
            Prerequisite::ParentRef => "PARENT-REF",
            Prerequisite::ParentAncestry => "PARENT-ANCESTRY",
            Prerequisite::DirtyWorktree => "DIRTY-WORKTREE",
            Prerequisite::ChildBranch => "CHILD-BRANCH",
        }
    }
}

/// A refusal raised **before** anything mutated the child branch.
#[derive(Debug)]
pub struct PlanError {
    pub prerequisite: Prerequisite,
    pub message: String,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.prerequisite.token(), self.message)
    }
}

/// Severity of a diagnostic the plan wants surfaced to the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
}

/// One line of operator-facing diagnostic, emitted on stderr by the CLI.
#[derive(Debug, Clone)]
pub struct Notice {
    pub level: Level,
    pub text: String,
}

/// What to reconcile. `child_branch` is resolved by the caller (the script
/// asks `gh pr view`), so nothing here needs the forge.
#[derive(Debug, Clone)]
pub struct PlanRequest<'a> {
    /// Any working tree of the repository; used for the read-only probes.
    pub repo_dir: &'a Path,
    /// The remote holding the default branch (`origin`).
    pub remote: &'a str,
    /// The default branch **name** — the `gh pr edit --base` target.
    pub default_branch: &'a str,
    /// The child PR's head branch.
    pub child_branch: &'a str,
    /// The parent PR's head branch name (may no longer resolve).
    pub parent_branch: &'a str,
}

/// An executable reconciliation, with every ambiguous name already resolved.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The default branch name — for forge retargeting and diagnostics ONLY.
    /// Never a git mutation target; see the module docs.
    pub default_branch: String,
    /// The pinned commit the child is replayed onto: the tip this run fetched.
    pub target_commit: String,
    /// The remote-tracking ref the fetch wrote, for diagnostics.
    pub target_ref: String,
    /// Where the rebase must run: the worktree holding the child branch when
    /// one does, else `repo_dir`.
    pub git_dir: PathBuf,
    /// `Some` when a linked worktree holds the child branch checked out.
    pub child_worktree: Option<PathBuf>,
    pub child_branch: String,
    /// The `rebase --onto <target> <upstream>` upstream: the parent branch
    /// name when it still resolves, else the pinned ref.
    pub parent_ref: String,
    /// `Some(ref)` only when the pinned-ref fallback was actually used, so the
    /// caller knows whether reaping the pin is its to do.
    pub parent_pin_ref: Option<String>,
    pub notices: Vec<Notice>,
}

fn git(dir: &Path, args: &[&str], timeout: Duration) -> CmdOutcome {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null());
    run_command(cmd, timeout)
}

fn resolves(dir: &Path, rev: &str) -> bool {
    git(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ],
        GIT_LOCAL_TIMEOUT,
    )
    .succeeded()
}

fn short(dir: &Path, rev: &str) -> String {
    git(dir, &["rev-parse", "--short", rev], GIT_LOCAL_TIMEOUT)
        .ok_stdout_trimmed()
        .unwrap_or_else(|| "?".to_string())
}

/// Fetch the default branch and resolve it to an immutable commit.
///
/// This is the whole fix in one function: the commit returned here is what the
/// rebase uses, and the local branch of the same name is never consulted. A
/// failure at either step is a refusal — reusing stale local state on a failed
/// fetch is the silent-data-loss path, not a graceful degradation.
fn fetch_and_pin_target(
    repo_dir: &Path,
    remote: &str,
    branch: &str,
) -> Result<(String, String), PlanError> {
    let target_ref = format!("refs/remotes/{remote}/{branch}");
    let refspec = format!("+refs/heads/{branch}:{target_ref}");
    let fetched = git(repo_dir, &["fetch", remote, &refspec], GIT_NET_TIMEOUT);
    if !fetched.succeeded() {
        let stderr = fetched.stderr_trimmed();
        // `couldn't find remote ref` is git's wording for "the remote has no
        // such branch" — a different operator problem (wrong
        // LOOM_DEFAULT_BRANCH, renamed default branch) from "the fetch could
        // not run", so it gets its own prerequisite rather than being folded
        // into a generic network failure.
        let missing = stderr.to_lowercase().contains("couldn't find remote ref");
        let prerequisite = if missing {
            Prerequisite::RemoteTarget
        } else {
            Prerequisite::Fetch
        };
        return Err(PlanError {
            prerequisite,
            message: format!(
                "Could not fetch '{branch}' from '{remote}': git exited non-zero.\n\
                 {stderr}\n\
                 Refusing rather than rebasing onto the LOCAL '{branch}', which is \
                 exactly the stale-destination bug this guard exists to prevent (#8583): \
                 a child whose files do not overlap the parent's would rebase cleanly \
                 and silently drop the merged parent's implementation."
            ),
        });
    }

    let commit = git(
        repo_dir,
        &["rev-parse", "--verify", &format!("{target_ref}^{{commit}}")],
        GIT_LOCAL_TIMEOUT,
    )
    .ok_stdout_trimmed()
    .filter(|s| !s.is_empty())
    .ok_or_else(|| PlanError {
        prerequisite: Prerequisite::RemoteTarget,
        message: format!(
            "Fetched '{branch}' from '{remote}', but '{target_ref}' does not resolve to a \
             commit. Refusing: with no verified remote tip there is no safe rebase \
             destination, and the local '{branch}' is not an acceptable substitute (#8583)."
        ),
    })?;

    Ok((commit, target_ref))
}

/// Locate the worktree that holds `child_branch` checked out, if any.
///
/// Git refuses to rebase a branch checked out in another worktree, and a Loom
/// child branch is ALWAYS checked out in its managed worktree (#3776), so the
/// rebase has to run *there*. An unreadable worktree list falls back to the
/// in-place path, which is what the shell's `|| true` did.
fn locate_child_worktree(repo_dir: &Path, child_branch: &str) -> Option<PathBuf> {
    let want = format!("refs/heads/{child_branch}");
    crate::worktree_ops::aggressive::enumerate_git_worktrees(repo_dir)
        .into_iter()
        .find(|wt| wt.branch.as_deref() == Some(want.as_str()))
        .map(|wt| wt.path)
}

/// Resolve the `rebase --onto <target> <upstream>` upstream.
///
/// Prefers the literal branch name while it still resolves (direct and legacy
/// invocations are unchanged), falling back to the `refs/loom/parent/<branch>`
/// pin `merge-pr.sh`'s merge-ordering guard writes before merging a stacked
/// parent (#7982). The pin's ancestry is **checked, not assumed** (#8010 item
/// 4): `rebase --onto <default> <non-ancestor> <child>` does not error, it
/// replays the wrong commit range.
fn resolve_parent_ref(
    dir: &Path,
    parent_branch: &str,
    child_branch: &str,
    notices: &mut Vec<Notice>,
) -> Result<(String, Option<String>), PlanError> {
    if resolves(dir, parent_branch) {
        return Ok((parent_branch.to_string(), None));
    }

    let pin = format!("refs/loom/parent/{parent_branch}");
    if !resolves(dir, &pin) {
        return Err(PlanError {
            prerequisite: Prerequisite::ParentRef,
            message: format!(
                "Branch '{parent_branch}' no longer resolves locally and no pinned ref \
                 {pin} exists either, so there is no upstream to replay the child's own \
                 commits from. Refusing before touching '{child_branch}'.\n\
                 If you know the parent's pre-merge tip, pin it and re-run:\n  \
                 git update-ref {pin} <the-parent-tip>"
            ),
        });
    }

    if !git(dir, &["merge-base", "--is-ancestor", &pin, child_branch], GIT_LOCAL_TIMEOUT)
        .succeeded()
    {
        return Err(PlanError {
            prerequisite: Prerequisite::ParentAncestry,
            message: format!(
                "Branch '{parent_branch}' no longer resolves locally, and the pinned ref \
                 {pin} ({}) is NOT an ancestor of '{child_branch}'.\n\n\
                 Rebasing onto a non-ancestor would not fail — it would replay the wrong \
                 commit range and silently add commits to the child PR. Refusing.\n\n\
                 This usually means the pin is stale: the branch name was reused by a later \
                 issue slice, and an older merge left the ref behind. Verify which parent \
                 this child was actually built on, then either:\n  \
                 git update-ref {pin} <the-correct-tip>\n  \
                 git update-ref -d {pin}   # then rebase by hand",
                short(dir, &pin)
            ),
        });
    }

    notices.push(Notice {
        level: Level::Warn,
        text: format!(
            "Branch '{parent_branch}' no longer resolves locally (likely deleted by \
             delete_branch_on_merge) — falling back to the pinned ref {pin}."
        ),
    });
    Ok((pin.clone(), Some(pin)))
}

/// Everything that must hold before the child branch is touched.
///
/// Every failure path returns [`PlanError`] having mutated nothing but
/// remote-tracking refs. Ordering is deliberate: the fetch/pin runs first, so
/// a network failure refuses before any local state is even inspected.
pub fn plan(req: &PlanRequest<'_>) -> Result<Plan, PlanError> {
    let mut notices = Vec::new();

    let (target_commit, target_ref) =
        fetch_and_pin_target(req.repo_dir, req.remote, req.default_branch)?;
    notices.push(Notice {
        level: Level::Info,
        text: format!(
            "Rebase destination: {target_commit} (fetched {}/{} -> {target_ref}). The LOCAL \
             '{}' is not consulted (#8583).",
            req.remote, req.default_branch, req.default_branch
        ),
    });

    // Advisory only: ask the remote LIVE (never a local remote-tracking ref,
    // which lingers stale after delete_branch_on_merge and false-warns on
    // every post-merge reconcile — #3776).
    if git(
        req.repo_dir,
        &[
            "ls-remote",
            "--exit-code",
            "--heads",
            req.remote,
            &format!("refs/heads/{}", req.parent_branch),
        ],
        GIT_NET_TIMEOUT,
    )
    .succeeded()
    {
        notices.push(Notice {
            level: Level::Warn,
            text: format!(
                "{}/{} still exists — confirm the parent PR has squash-merged before \
                 reconciling. (delete-branch-on-merge normally removes it once the parent \
                 merges.)",
                req.remote, req.parent_branch
            ),
        });
    }

    let child_worktree = locate_child_worktree(req.repo_dir, req.child_branch);
    let git_dir = match &child_worktree {
        Some(wt) => {
            notices.push(Notice {
                level: Level::Info,
                text: format!(
                    "Child branch {} is checked out in worktree: {} — running the rebase there.",
                    req.child_branch,
                    wt.display()
                ),
            });
            wt.clone()
        }
        None => req.repo_dir.to_path_buf(),
    };

    if !resolves(&git_dir, req.child_branch) {
        return Err(PlanError {
            prerequisite: Prerequisite::ChildBranch,
            message: format!(
                "Child branch '{}' does not resolve in {}. Fetch or check out the child \
                 branch before reconciling.",
                req.child_branch,
                git_dir.display()
            ),
        });
    }

    let dirty = git(&git_dir, &["status", "--porcelain"], GIT_LOCAL_TIMEOUT).stdout_trimmed();
    if !dirty.is_empty() {
        return Err(PlanError {
            prerequisite: Prerequisite::DirtyWorktree,
            message: format!(
                "Working tree is dirty ({}). Commit, stash, or discard changes before \
                 reconciling.",
                git_dir.display()
            ),
        });
    }

    let (parent_ref, parent_pin_ref) =
        resolve_parent_ref(&git_dir, req.parent_branch, req.child_branch, &mut notices)?;

    Ok(Plan {
        default_branch: req.default_branch.to_string(),
        target_commit,
        target_ref,
        git_dir,
        child_worktree,
        child_branch: req.child_branch.to_string(),
        parent_ref,
        parent_pin_ref,
        notices,
    })
}

/// The `git rebase --onto` this whole module exists to get right.
///
/// `Err` carries git's own output; the rebase is deliberately left in
/// progress (not aborted) so the operator's documented
/// `git rebase --continue` recovery still works.
pub fn rebase(plan: &Plan) -> Result<(), String> {
    let outcome = git(
        &plan.git_dir,
        &[
            "rebase",
            "--onto",
            &plan.target_commit,
            &plan.parent_ref,
            &plan.child_branch,
        ],
        GIT_LOCAL_TIMEOUT,
    );
    if outcome.succeeded() {
        return Ok(());
    }
    Err(format!("{}\n{}", outcome.stdout_trimmed(), outcome.stderr_trimmed())
        .trim()
        .to_string())
}

/// Single-quote a value for `eval` in POSIX shell.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The plan as `eval`-able shell assignments — the script's stdout contract.
#[must_use]
pub fn render_shell(plan: &Plan) -> String {
    let worktree = plan
        .child_worktree
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let pin = plan.parent_pin_ref.clone().unwrap_or_default();
    format!(
        "LOOM_RS_TARGET_COMMIT={}\n\
         LOOM_RS_TARGET_REF={}\n\
         LOOM_RS_GIT_DIR={}\n\
         LOOM_RS_CHILD_WORKTREE={}\n\
         LOOM_RS_PARENT_REF={}\n\
         LOOM_RS_PARENT_PIN_REF={}\n",
        sh_quote(&plan.target_commit),
        sh_quote(&plan.target_ref),
        sh_quote(&plan.git_dir.display().to_string()),
        sh_quote(&worktree),
        sh_quote(&plan.parent_ref),
        sh_quote(&pin),
    )
}
