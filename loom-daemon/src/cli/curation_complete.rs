//! Complete attended or daemon-mediated curation using one native producer.
use anyhow::{Context, Result};
use chrono::Utc;
use loom_daemon::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
use loom_daemon::telemetry::kinds::issue_estimate::completion::{self, Completion};
use std::path::PathBuf;

#[derive(clap::Args)]
pub(crate) struct CurationCompleteArgs {
    /// owner/repo whose CURRENT issue body is fetched.
    #[arg(long)]
    repo: String,
    #[arg(long)]
    issue: u32,
    /// Actual Curator model, never the Builder model.
    #[arg(long)]
    model: String,
    /// Actual Curator runtime.
    #[arg(long)]
    runtime: String,
    /// UUID for this completion. Keep it on retry; new UUID on re-curation.
    #[arg(long)]
    completion_id: String,
    /// Main workspace monitored by the daemon, not an issue worktree.
    #[arg(long)]
    workspace_root: PathBuf,
}

impl CurationCompleteArgs {
    pub(crate) fn run(self) -> Result<()> {
        self.run_with(std::ffi::OsStr::new("gh"))
    }

    fn run_with(self, gh: &std::ffi::OsStr) -> Result<()> {
        let host = loom_daemon::sweep_registry::host_identity();
        let mut request = Completion {
            repo: &self.repo,
            issue: self.issue,
            model: &self.model,
            runtime: &self.runtime,
            id: &self.completion_id,
            host_id: &host,
            at: Utc::now(),
        };
        if completion::existing(&self.workspace_root, &request)?.is_some() {
            println!(
                "{}",
                serde_json::json!({"recorded":true,"retry":true,"completion_id":self.completion_id})
            );
            return Ok(());
        }
        let outcome = GhInvocation::new(
            Operation::new("curation_complete.issue_body"),
            AccessIntent::Read,
            GhTarget::repo(&self.repo).map_err(anyhow::Error::msg)?,
            std::time::Duration::from_secs(30),
        )
        .forge_op(loom_daemon::forge_call_stats::ops::ISSUE_VIEW_STATE)
        .program(gh)
        .current_dir(&self.workspace_root)
        .args(["api", &format!("repos/{}/issues/{}", self.repo, self.issue)])
        .run();
        let output = outcome
            .ok_output()
            .context("cannot fetch current curated issue")?;
        let issue: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let body = issue.get("body").and_then(|v| v.as_str()).unwrap_or("");
        // The current marker is knowable only after the forge read completed.
        request.at = Utc::now();
        let saved = completion::record(&self.workspace_root, body, &request)?;
        anyhow::ensure!(saved.is_some(), "new curation requires a concrete estimate marker");
        println!("{}", serde_json::json!({"recorded":true,"completion_id":self.completion_id}));
        Ok(())
    }
}

#[cfg(all(test, unix))]
#[path = "curation_complete_tests.rs"]
mod tests;
