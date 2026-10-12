//! Complete attended or daemon-mediated curation using one native producer.
use anyhow::{Context, Result};
use chrono::Utc;
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
        let host = loom_daemon::sweep_registry::host_identity();
        let request = Completion {
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
        let output = std::process::Command::new("gh")
            .args(["api", &format!("repos/{}/issues/{}", self.repo, self.issue)])
            .output()
            .context("cannot fetch current curated issue")?;
        anyhow::ensure!(output.status.success(), "current issue fetch failed");
        let issue: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let body = issue.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let saved = completion::record(&self.workspace_root, body, &request)?;
        anyhow::ensure!(saved.is_some(), "new curation requires a concrete estimate marker");
        println!("{}", serde_json::json!({"recorded":true,"completion_id":self.completion_id}));
        Ok(())
    }
}
