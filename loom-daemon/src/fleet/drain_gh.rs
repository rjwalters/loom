//! The production [`ClaimResetter`] for `fleet drain` (#4343), split out of
//! the frozen `drain.rs` when its three `gh` spawns moved onto the
//! [`crate::gh_invocation`] facade (#9985).

use super::ClaimResetter;
use crate::gh_invocation::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation};
use crate::proc_exec::Completion;
use anyhow::{Context, Result};
use std::process::Output;

/// The production [`ClaimResetter`]: `gh issue view` to check the current
/// label set, then `gh issue edit` + `gh issue comment` to flip it — run
/// **locally** (never over SSH; the forge is global).
///
/// Each `gh` invocation is given an explicit `PATH` env (via
/// [`crate::fleet::path_bootstrap::local_gh_path_env`], the facade's
/// [`GhInvocation::path_env`]) rather than relying on this process's
/// inherited environment (#4831): a `loom-daemon` launched non-interactively
/// (launchd/systemd) may not have `gh`/Homebrew on its inherited PATH even
/// though an interactive login shell on the same host would.
pub struct GhClaimResetter;

/// Run one captured `gh` call against `repo`.
///
/// #5431: this resetter targets an arbitrary fleet repo by `--repo
/// <owner/repo>` with no checkout-root `current_dir`, so the credential is
/// keyed off the owner slug — the facade's typed-target `GH_CONFIG_DIR`
/// lookup. For a cross-owner repo this picks that owner's installation token;
/// without it the label edit/comment (writes) would silently 404 under the
/// root owner's token. A no-op for a single-owner fleet.
fn run_gh(
    repo: &str,
    gh_path: &str,
    op: &'static str,
    intent: AccessIntent,
    args: &[&str],
) -> Result<Output> {
    let target = GhTarget::repo(repo).map_err(|e| anyhow::anyhow!(e))?;
    let inv =
        GhInvocation::new(Operation::new(op), intent, target, crate::forge_cmd::FORGE_CMD_TIMEOUT)
            .path_env(gh_path)
            .args(args);
    match inv.execute()? {
        GhCompletion::Captured(Completion::Exited(out)) => Ok(out),
        GhCompletion::Captured(Completion::TimedOut { .. }) => anyhow::bail!("timed out"),
        GhCompletion::Passthrough(_) => anyhow::bail!("unexpected passthrough"),
    }
}

impl ClaimResetter for GhClaimResetter {
    fn reset_claim(&self, repo: &str, issue: u32, host: &str) -> Result<bool> {
        let gh_path = crate::fleet::path_bootstrap::local_gh_path_env();
        let issue_s = issue.to_string();
        let view = run_gh(
            repo,
            &gh_path,
            "claim_reset.issue_view",
            AccessIntent::Read,
            &[
                "issue", "view", &issue_s, "--repo", repo, "--json", "labels",
            ],
        )
        .with_context(|| format!("gh issue view #{issue} in {repo}"))?;
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

        let edit = run_gh(
            repo,
            &gh_path,
            "claim_reset.issue_edit",
            AccessIntent::Write,
            &[
                "issue",
                "edit",
                &issue_s,
                "--repo",
                repo,
                "--remove-label",
                "loom:building",
                "--add-label",
                "loom:issue",
            ],
        )
        .with_context(|| format!("gh issue edit #{issue} in {repo}"))?;
        if !edit.status.success() {
            anyhow::bail!(
                "gh issue edit #{issue} in {repo} failed: {}",
                String::from_utf8_lossy(&edit.stderr).trim()
            );
        }

        // Best-effort comment — never fails the reset itself (mirrors the
        // rest of Loom's "a forge comment is advisory" posture).
        let body = format!(
            "🔧 **fleet drain**: host `{host}` was drained/retired while this issue was \
             claimed; `loom:building` reset to `loom:issue` so it is not stranded (see \
             epic #4340, #4343)."
        );
        let _ = run_gh(
            repo,
            &gh_path,
            "claim_reset.issue_comment",
            AccessIntent::Write,
            &[
                "issue", "comment", &issue_s, "--repo", repo, "--body", &body,
            ],
        );

        Ok(true)
    }
}
