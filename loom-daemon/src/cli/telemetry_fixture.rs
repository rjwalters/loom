//! Offline synthetic inputs, not evidence of a completed backend trial.
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use loom_daemon::telemetry::fixture;
use std::io::Write;
use std::path::PathBuf;

#[derive(clap::Args)]
pub(crate) struct FixtureArgs {
    /// New directory for envelopes.jsonl and manifest.json; never overwritten.
    #[arg(long)]
    output: PathBuf,
    /// Stable namespace; repeat it to test duplicate delivery, change it for a new trial.
    #[arg(long)]
    run_id: String,
    /// Fixed RFC3339 anchor. Set inside the backend's query/retention window.
    #[arg(long, default_value = "2026-09-21T12:00:00Z")]
    start_time: DateTime<Utc>,
}

impl FixtureArgs {
    pub(crate) fn run(self) -> Result<()> {
        let bundle = fixture::build(&self.run_id, self.start_time)?;
        // Build/validate entirely before creating output. Publish the complete
        // manifest last; its absence identifies an interrupted publication.
        let parent = self
            .output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        anyhow::ensure!(!self.output.exists(), "fixture output already exists");
        let temp = tempfile::tempdir_in(parent).context("fixture parent must exist")?;
        let mut envelopes = std::fs::File::create(temp.path().join("envelopes.jsonl"))?;
        for envelope in &bundle.envelopes {
            serde_json::to_writer(&mut envelopes, envelope)?;
            envelopes.write_all(b"\n")?;
        }
        envelopes.sync_all()?;
        let mut manifest = std::fs::File::create(temp.path().join("manifest.json"))?;
        serde_json::to_writer_pretty(&mut manifest, &bundle.manifest)?;
        manifest.write_all(b"\n")?;
        manifest.sync_all()?;
        // create_dir is the no-clobber reservation; move only into our directory.
        std::fs::create_dir(&self.output)
            .context("fixture output already exists or is unavailable")?;
        for name in ["envelopes.jsonl", "manifest.json"] {
            std::fs::rename(temp.path().join(name), self.output.join(name))?;
        }
        println!(
            "{}",
            serde_json::json!({"synthetic":true,"output":self.output,"envelopes":bundle.envelopes.len(),"backend_verification":"not_performed"})
        );
        Ok(())
    }
}
