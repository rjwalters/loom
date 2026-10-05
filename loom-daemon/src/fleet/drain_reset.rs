//! The production [`ClaimResetter`] for `fleet drain` (moved out of
//! `drain.rs`; #10089 routed its three `gh` spawns through the facade).

use anyhow::{Context, Result};
use std::ffi::{OsStr, OsString};
use std::time::Duration;

use super::ClaimResetter;
use crate::cmd_out::CmdOutcome;
use crate::forge_call_stats::{ops, ForgeOp};
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

/// Deadline for each reset call. The raw spawns had none; this is generous
/// so only a wedged forge — not a slow one — trips it.
const GH_TIMEOUT: Duration = Duration::from_secs(120);

/// A single-issue label read: no inventoried operation covers it.
const ISSUE_LABELS: ForgeOp =
    ForgeOp::uninventoried("single-issue label read has no inventory row");

/// The production [`ClaimResetter`]: `gh issue view` to check the current
/// label set, then `gh issue edit` + `gh issue comment` to flip it — run
/// **locally** (never over SSH; the forge is global).
///
/// Each `gh` invocation goes through [`GhInvocation`] (#10089), so it is one
/// counted `forge_call_stats` row, and is given an explicit `PATH` (via
/// [`crate::fleet::path_bootstrap::local_gh_path_env`]) rather than relying
/// on this process's inherited environment (#4831): a `loom-daemon` launched
/// non-interactively (launchd/systemd) may not have `gh`/Homebrew on its
/// inherited PATH even though an interactive login shell on the same host
/// would.
pub struct GhClaimResetter;

impl ClaimResetter for GhClaimResetter {
    fn reset_claim(&self, repo: &str, issue: u32, host: &str) -> Result<bool> {
        let path = crate::fleet::path_bootstrap::local_gh_path_env();
        reset_claim_with(OsStr::new("gh"), path.into(), repo, issue, host)
    }
}

/// One reset call. #5431: the resetter targets an arbitrary fleet repo by
/// `--repo <owner/repo>` with no checkout-root `current_dir`, so the typed
/// [`GhTarget`] keys the credential off the owner slug — for a cross-owner
/// repo that owner's installation token; without it the label edit/comment
/// (writes) would silently 404 under the root owner's token. A slug the
/// target rejects keeps running with the ambient credential, as before.
fn call(
    name: &'static str,
    intent: AccessIntent,
    op: ForgeOp,
    program: &OsStr,
    path: &OsString,
    repo: &str,
) -> GhInvocation {
    let target = GhTarget::repo(repo).unwrap_or(GhTarget::None);
    GhInvocation::new(Operation::new(name), intent, target, GH_TIMEOUT)
        .forge_op(op)
        .program(program)
        .child_path(path.clone())
}

/// [`GhClaimResetter::reset_claim`] with the `gh` program and child `PATH`
/// injected (the test seam). A bare `gh` goes through the facade's resolver.
fn reset_claim_with(
    program: &OsStr,
    path: OsString,
    repo: &str,
    issue: u32,
    host: &str,
) -> Result<bool> {
    let number = issue.to_string();
    let view = call("drain.issue_labels", AccessIntent::Read, ISSUE_LABELS, program, &path, repo)
        .args(["issue", "view", &number, "--repo", repo, "--json", "labels"])
        .run();
    let view = match view {
        CmdOutcome::Ran(out) => out,
        CmdOutcome::Unavailable(u) => anyhow::bail!("gh issue view #{issue} in {repo}: {u}"),
    };
    if !view.status.success() {
        anyhow::bail!(
            "gh issue view #{issue} in {repo} failed: {}",
            String::from_utf8_lossy(&view.stderr).trim()
        );
    }
    let parsed: serde_json::Value =
        serde_json::from_slice(&view.stdout).context("parsing gh issue view --json labels")?;
    let has_building = parsed["labels"]
        .as_array()
        .is_some_and(|labels| labels.iter().any(|l| l["name"] == "loom:building"));
    if !has_building {
        return Ok(false);
    }

    let edit = call(
        "drain.reset_labels",
        AccessIntent::Write,
        ops::ISSUE_EDIT_LABELS,
        program,
        &path,
        repo,
    )
    .args(["issue", "edit", &number, "--repo", repo])
    .args([
        "--remove-label",
        "loom:building",
        "--add-label",
        "loom:issue",
    ])
    .run();
    let edit = match edit {
        CmdOutcome::Ran(out) => out,
        CmdOutcome::Unavailable(u) => anyhow::bail!("gh issue edit #{issue} in {repo}: {u}"),
    };
    if !edit.status.success() {
        anyhow::bail!(
            "gh issue edit #{issue} in {repo} failed: {}",
            String::from_utf8_lossy(&edit.stderr).trim()
        );
    }

    // Best-effort comment — never fails the reset itself (mirrors the rest of
    // Loom's "a forge comment is advisory" posture).
    let body = format!(
        "🔧 **fleet drain**: host `{host}` was drained/retired while this issue was claimed; \
         `loom:building` reset to `loom:issue` so it is not stranded (see epic #4340, #4343)."
    );
    let _ = call(
        "drain.reset_comment",
        AccessIntent::Write,
        ops::COMMENT_CREATE,
        program,
        &path,
        repo,
    )
    .args(["issue", "comment", &number, "--repo", repo, "--body", &body])
    .run();

    Ok(true)
}

#[cfg(test)]
#[path = "drain_reset_tests.rs"]
mod tests;
