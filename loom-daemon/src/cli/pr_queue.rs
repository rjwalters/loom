use anyhow::{Context, Result};
use loom_daemon::{
    comment_trust::TrustPolicy,
    pr_planning::{self, PrRole},
};
use std::path::PathBuf;

#[derive(clap::Args)]
pub(crate) struct PrQueueArgs {
    #[arg(long, value_enum)]
    role: PrRole,
    #[arg(long, default_value = ".")]
    repo_root: PathBuf,
    /// Offline REST pull array: policy preview only, without live fallback guard.
    #[arg(long)]
    input: Option<PathBuf>,
}

impl PrQueueArgs {
    pub(crate) fn run(mut self) -> Result<()> {
        self.repo_root =
            std::fs::canonicalize(&self.repo_root).context("resolve repository root")?;
        let rows = if let Some(input) = self.input {
            let rows = serde_json::from_slice(&std::fs::read(input)?).context("parse PR array")?;
            pr_planning::ordered_queue(
                rows,
                self.role,
                pr_planning::prefer_human_prs(&self.repo_root),
                &TrustPolicy::for_root(&self.repo_root),
            )
        } else {
            let gh =
                std::env::var_os("LOOM_GH_BIN").map_or_else(|| PathBuf::from("gh"), PathBuf::from);
            pr_planning::fetch_queue(&self.repo_root, &gh, self.role)?
        };
        let rows: Vec<_> = rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "number": r["number"], "title": r["title"], "origin": r["origin"],
                    "priorityReason": r["priorityReason"], "mode": r["mode"]
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&rows)?);
        Ok(())
    }
}
