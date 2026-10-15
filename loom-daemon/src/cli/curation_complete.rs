//! Complete attended or daemon-mediated curation using one native producer.
//!
//! This is also the points gate (#11663, `docs/design/issue-sizing.md` §5d):
//! it resolves the story points from the issue body and its trusted comments,
//! refuses with no label when they are missing or invalid, and otherwise
//! projects `points:N` (removing any other `points:*` label) and applies
//! `loom:curated`. It writes no telemetry: the daemon emits `issue.estimate`
//! from the curated issue it observes (#12118).
use anyhow::{Context, Result};
use loom_daemon::comment_trust::TrustPolicy;
use loom_daemon::forge_call_stats::ops::{self, ForgeOp};
use loom_daemon::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
use loom_daemon::points_marker::resolve_points;
use loom_daemon::story_points::POINTS_LABEL_PREFIX;
use std::path::PathBuf;

#[derive(clap::Args)]
pub(crate) struct CurationCompleteArgs {
    /// owner/repo whose CURRENT issue body is fetched.
    #[arg(long)]
    repo: String,
    #[arg(long)]
    issue: u32,
    /// Accepted for compatibility; ignored since the daemon emits the fact (#12118).
    #[arg(long)]
    #[allow(dead_code)]
    model: String,
    /// Accepted for compatibility; ignored since the daemon emits the fact (#12118).
    #[arg(long)]
    #[allow(dead_code)]
    runtime: String,
    /// Accepted for compatibility; echoed in the output, otherwise ignored.
    #[arg(long)]
    completion_id: String,
    /// Main workspace monitored by the daemon, not an issue worktree.
    #[arg(long)]
    workspace_root: PathBuf,
}

impl CurationCompleteArgs {
    pub(crate) fn run(self) -> Result<()> {
        let policy = TrustPolicy::for_root(&self.workspace_root);
        // The label writes go to the explicit --repo; vet it first (#9548).
        let (root, repo) = (self.workspace_root.clone(), self.repo.clone());
        let may_write = move || match loom_daemon::write_scope::may_write_from(&root, Some(&repo)) {
            loom_daemon::write_scope::Verdict::Allow(_) => Ok(()),
            loom_daemon::write_scope::Verdict::Deny(why) => Err(why),
        };
        self.run_with(std::ffi::OsStr::new("gh"), &policy, &may_write)
    }

    fn gh(
        &self,
        gh: &std::ffi::OsStr,
        op: &'static str,
        forge_op: ForgeOp,
        intent: AccessIntent,
        args: &[&str],
    ) -> Result<std::process::Output> {
        let outcome = GhInvocation::new(
            Operation::new(op),
            intent,
            GhTarget::repo(&self.repo).map_err(anyhow::Error::msg)?,
            std::time::Duration::from_secs(30),
        )
        .forge_op(forge_op)
        .program(gh)
        .current_dir(&self.workspace_root)
        .args(args)
        .run();
        match outcome.into_result() {
            Ok(o) if o.status.success() => Ok(o),
            Ok(o) => anyhow::bail!("{op}: {}", String::from_utf8_lossy(&o.stderr).trim()),
            Err(e) => anyhow::bail!("{op}: {e}"),
        }
    }

    fn issue_json(&self, gh: &std::ffi::OsStr) -> Result<serde_json::Value> {
        let path = format!("repos/{}/issues/{}", self.repo, self.issue);
        let output = self
            .gh(
                gh,
                "curation_complete.issue_body",
                ops::ISSUE_VIEW_STATE,
                AccessIntent::Read,
                &["api", &path],
            )
            .context("cannot fetch current curated issue")?;
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    /// Add `points:N` and `loom:curated`, then remove every other `points:*`
    /// label the issue carries. Every write is idempotent.
    fn project_labels(
        &self,
        gh: &std::ffi::OsStr,
        issue: &serde_json::Value,
        points: u32,
    ) -> Result<()> {
        let wanted = format!("{POINTS_LABEL_PREFIX}{points}");
        let path = format!("repos/{}/issues/{}/labels", self.repo, self.issue);
        let add = [
            format!("labels[]={wanted}"),
            "labels[]=loom:curated".to_string(),
        ];
        let args = [
            "api", "-X", "POST", &path, "-f", &add[0], "-f", &add[1], "--silent",
        ];
        self.gh(
            gh,
            "curation_complete.add_labels",
            ops::ISSUE_EDIT_LABELS,
            AccessIntent::Write,
            &args,
        )?;
        let stale = issue
            .get("labels")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|l| l.get("name").and_then(serde_json::Value::as_str))
            .filter(|name| name.starts_with(POINTS_LABEL_PREFIX) && *name != wanted);
        for name in stale {
            let path = format!("{path}/{}", name.replace(':', "%3A"));
            let args = ["api", "-X", "DELETE", &path, "--silent"];
            if let Err(e) = self.gh(
                gh,
                "curation_complete.remove_label",
                ops::ISSUE_EDIT_LABELS,
                AccessIntent::Write,
                &args,
            ) {
                // Already absent is the desired end state.
                if !format!("{e:#}").contains("404") {
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn run_with(
        self,
        gh: &std::ffi::OsStr,
        policy: &TrustPolicy,
        may_write: &dyn Fn() -> std::result::Result<(), String>,
    ) -> Result<()> {
        // Refuse before anything is labeled.
        may_write().map_err(|why| anyhow::anyhow!("refusing to label {}: {why}", self.repo))?;
        let issue = self.issue_json(gh)?;
        let path = format!("repos/{}/issues/{}/comments", self.repo, self.issue);
        let listing = self
            .gh(
                gh,
                "curation_complete.issue_comments",
                ops::COMMENT_LIST,
                AccessIntent::Read,
                &["api", &path, "--paginate"],
            )
            .context("cannot fetch the issue's comments")?;
        let comments = policy
            .trusted_bodies(&listing.stdout)
            .context("cannot read the issue's comment listing")?;
        let body = issue.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let points = resolve_points(body, policy.trusts_json(&issue), &comments)
            .map_err(|e| anyhow::anyhow!("issue {}#{}: {e}", self.repo, self.issue))?;
        self.project_labels(gh, &issue, points)?;
        println!(
            "{}",
            serde_json::json!({"recorded":true,"points":points,"completion_id":self.completion_id})
        );
        Ok(())
    }
}

#[cfg(all(test, unix))]
#[path = "curation_complete_tests.rs"]
mod tests;
