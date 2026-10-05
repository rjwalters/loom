//! The pre-delete stacked-child retarget (#9372): before `merge-pr.sh` deletes
//! a merged parent's remote branch, move every open PR still based on it onto
//! the parent's own base — or keep the branch.
//!
//! # The failure it closes
//!
//! `merge-pr.sh` deletes the merged head branch with a bare ref delete
//! (`DELETE repos/<nwo>/git/refs/heads/<branch>`) whenever the repo does not
//! auto-delete on merge. GitHub retargets open children only inside its own
//! delete-on-merge flow; a bare ref delete instead **closes** every open PR
//! whose base was that branch (`base_ref_deleted`), and the closure is
//! unrecoverable through the API — `gh pr edit --base` refuses a closed PR, and
//! `gh pr reopen` refuses one whose base no longer exists.
//!
//! The post-merge reconcile pass (`_auto_reconcile_stacked_children`) retargets
//! the children it reconciles, but it deliberately *defers* a child whose issue
//! still carries `loom:building` (#9259) — so exactly the children a Builder
//! is still working on were left on the parent branch when the delete ran.
//! Three real incidents followed that shape.
//!
//! # Why a fresh query, not the pre-merge snapshot
//!
//! `STACKED_CHILDREN_JSON` is a snapshot taken before the merge, and a reconcile
//! that ran in between may have retargeted some children already, while a new
//! child may have been opened. The only question that matters at delete time is
//! "does any open PR target this branch *now*", so this verb asks it live — and
//! asks it again after retargeting, so the delete never runs on an answer that
//! predates its own mutations.
//!
//! # Fail direction
//!
//! The delete is the irreversible step and leaving the branch is free, so every
//! uncertainty resolves to [`Verdict::Keep`]: an unanswered or unreadable child
//! query, a row without a PR number, a failed retarget, no known retarget base,
//! or a child still present on the re-check. `--allow-stacked-children` does not
//! reach this verb; it bypasses only the pre-merge ordering guard, whose job is
//! different.
//!
//! The no-children path prints nothing and returns [`Verdict::Delete`], so a
//! merge with no stacked children behaves exactly as before.

use crate::cmd_out::CmdOutcome;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

/// Upper bound on one child listing. A parent with more open children than
/// this still cannot be deleted early: the post-retarget re-check sees the
/// remainder and keeps the branch.
pub const LIST_LIMIT: &str = "100";

/// One open PR still based on the parent branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    pub number: i64,
    pub head_ref_name: String,
}

/// What the caller may do with the parent branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No open PR targets the branch (now): the delete may proceed.
    Delete,
    /// Deleting would close at least one open PR, or that could not be ruled
    /// out: leave the branch in place.
    Keep,
}

/// Severity of one operator-facing line, replayed by the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warning,
}

/// The verdict plus the lines explaining it, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub verdict: Verdict,
    pub lines: Vec<(Level, String)>,
}

/// The two forge operations this decision needs, injectable for tests.
pub trait Forge {
    /// Open PRs whose base is `branch`. `Err` means "no usable answer".
    fn open_children(&self, repo: &str, branch: &str) -> Result<Vec<Child>, String>;
    /// Change PR `number`'s base to `base`.
    fn retarget(&self, repo: &str, number: i64, base: &str) -> Result<(), String>;
}

/// Parse `gh pr list --json number,headRefName` strictly.
///
/// Unlike [`super::stacked_children::parse_children`] (which fails OPEN to an
/// empty list), anything other than an array of rows that each carry a PR
/// number is an error here: reading an unparseable answer as "no children"
/// is precisely what would authorize the delete.
///
/// # Errors
///
/// When the input is not a JSON array, or a row has no integer `number`.
pub fn parse_children_strict(json: &str) -> Result<Vec<Child>, String> {
    let value: serde_json::Value = serde_json::from_str(json.trim())
        .map_err(|e| format!("the child-PR listing was not JSON ({e})"))?;
    let serde_json::Value::Array(rows) = value else {
        return Err("the child-PR listing was not a JSON array".to_string());
    };
    rows.iter()
        .map(|row| {
            let number = row
                .get("number")
                .and_then(serde_json::Value::as_i64)
                .ok_or_else(|| "a child-PR row had no PR number".to_string())?;
            let head_ref_name = row
                .get("headRefName")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(Child {
                number,
                head_ref_name,
            })
        })
        .collect()
}

fn child_list(children: &[Child]) -> String {
    children
        .iter()
        .map(|c| format!("#{}", c.number))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The one keep-the-branch warning, so every refusal names the same remedy.
fn keep_message(repo: &str, branch: &str, base: &str, reason: &str) -> String {
    let base = if base.is_empty() {
        "<default-branch>"
    } else {
        base
    };
    format!(
        "Keeping remote branch '{branch}': {reason}. Deleting it now could make GitHub close any open PR still based on it, and a PR closed that way can be neither reopened nor retargeted (#9372). Retarget each open child by hand (gh pr edit <child> --repo {repo} --base {base}), confirm none remain (gh pr list --repo {repo} --base {branch} --state open), then delete it: gh api -X DELETE repos/{repo}/git/refs/heads/{branch}"
    )
}

fn keep(
    repo: &str,
    branch: &str,
    base: &str,
    reason: &str,
    mut lines: Vec<(Level, String)>,
) -> Report {
    lines.push((Level::Warning, keep_message(repo, branch, base, reason)));
    Report {
        verdict: Verdict::Keep,
        lines,
    }
}

/// Decide, retargeting as needed, whether `branch` may be deleted.
///
/// `base` is where each child is moved — the merged parent's own base, which
/// is where the parent's commits now live.
pub fn prepare_delete(forge: &impl Forge, repo: &str, branch: &str, base: &str) -> Report {
    let children = match forge.open_children(repo, branch) {
        Ok(c) => c,
        Err(e) => {
            let reason = format!("could not list the open PRs based on it ({e})");
            return keep(repo, branch, base, &reason, Vec::new());
        }
    };
    if children.is_empty() {
        return Report {
            verdict: Verdict::Delete,
            lines: Vec::new(),
        };
    }

    let list = child_list(&children);
    if base.is_empty() || base == branch {
        let reason = format!(
            "{} open stacked child PR(s) ({list}) still target it and no retarget base is known",
            children.len()
        );
        return keep(repo, branch, base, &reason, Vec::new());
    }

    let mut lines = Vec::new();
    let mut failed = Vec::new();
    for child in &children {
        match forge.retarget(repo, child.number, base) {
            Ok(()) => lines.push((
                Level::Info,
                format!(
                    "Retargeted stacked child PR #{} ({}) from '{branch}' to '{base}' before deleting '{branch}' (#9372). If it still needs rebasing onto the merged parent, run: ./.loom/scripts/reconcile-stack.sh {} {branch}",
                    child.number,
                    if child.head_ref_name.is_empty() { "?" } else { &child.head_ref_name },
                    child.number,
                ),
            )),
            Err(e) => failed.push(format!("#{} ({e})", child.number)),
        }
    }
    if !failed.is_empty() {
        let reason =
            format!("could not retarget stacked child PR(s) {} to '{base}'", failed.join(", "));
        return keep(repo, branch, base, &reason, lines);
    }

    match forge.open_children(repo, branch) {
        Ok(rest) if rest.is_empty() => Report {
            verdict: Verdict::Delete,
            lines,
        },
        Ok(rest) => {
            let reason = format!(
                "open PR(s) {} still target it after retargeting {list}",
                child_list(&rest)
            );
            keep(repo, branch, base, &reason, lines)
        }
        Err(e) => {
            let reason = format!(
                "could not re-check the open PRs based on it after retargeting {list} ({e})"
            );
            keep(repo, branch, base, &reason, lines)
        }
    }
}

/// The live forge: uncached `gh`, resolved by the facade's usual ladder
/// (policy launcher → `LOOM_GH_BIN` → `PATH`).
pub struct GhForge;

fn stderr_tail(o: &std::process::Output) -> String {
    let s = String::from_utf8_lossy(&o.stderr);
    let s = s.trim();
    if s.is_empty() {
        format!("gh exited {}", o.status.code().unwrap_or(-1))
    } else {
        s.lines().last().unwrap_or(s).to_string()
    }
}

impl Forge for GhForge {
    fn open_children(&self, repo: &str, branch: &str) -> Result<Vec<Child>, String> {
        let out = GhInvocation::new(
            Operation::new("merge_pr.retarget_children.list"),
            AccessIntent::Read,
            GhTarget::None,
            std::time::Duration::from_secs(60),
        )
        .args([
            "pr", "list", "--repo", repo, "--base", branch, "--state", "open",
        ])
        .args(["--json", "number,headRefName", "--limit", LIST_LIMIT])
        .run();
        match out {
            CmdOutcome::Ran(o) if o.status.success() => {
                parse_children_strict(&String::from_utf8_lossy(&o.stdout))
            }
            CmdOutcome::Ran(o) => Err(stderr_tail(&o)),
            CmdOutcome::Unavailable(u) => Err(format!("gh did not answer: {u:?}")),
        }
    }

    fn retarget(&self, repo: &str, number: i64, base: &str) -> Result<(), String> {
        let out = GhInvocation::new(
            Operation::new("merge_pr.retarget_children.edit"),
            AccessIntent::Write,
            GhTarget::None,
            std::time::Duration::from_secs(60),
        )
        .args([
            "pr",
            "edit",
            &number.to_string(),
            "--repo",
            repo,
            "--base",
            base,
        ])
        .run();
        match out {
            CmdOutcome::Ran(o) if o.status.success() => Ok(()),
            CmdOutcome::Ran(o) => Err(stderr_tail(&o)),
            CmdOutcome::Unavailable(u) => Err(format!("gh did not answer: {u:?}")),
        }
    }
}

#[cfg(test)]
mod tests;
