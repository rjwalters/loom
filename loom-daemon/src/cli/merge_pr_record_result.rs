//! `loom-daemon merge-pr record-result` (#12029): the terminal result of a
//! sweep's merge phase, called by `merge-pr.sh` once on each terminal path.
//!
//! Telemetry only. It closes the open sweep `merge` attempt with the PR, head
//! and result (`loom_daemon::observability::lifecycle::merge_result`), does
//! nothing when no merge attempt is open, and **always exits 0**: a failed
//! record never changes `merge-pr.sh`'s own exit code. Fail-open: a daemon
//! predating this verb answers a clap usage error that the script ignores.

use anyhow::Result;
use loom_daemon::observability::lifecycle::merge_result::{record, MergeResult};

#[derive(clap::Args)]
pub(crate) struct RecordResultArgs {
    /// The PR `merge-pr.sh` was merging.
    #[arg(long)]
    pr: u32,
    /// The PR head at the terminal path, when known.
    #[arg(long)]
    head: Option<String>,
    /// `merged`, `refused` or `failed`.
    #[arg(long)]
    result: String,
    /// A `classify-response` token, when the failure had one.
    #[arg(long)]
    reason: Option<String>,
    /// `merge-pr.sh`'s exit code on this path.
    #[arg(long, allow_hyphen_values = true)]
    exit_code: Option<i32>,
}

impl RecordResultArgs {
    pub(crate) fn run(self) -> Result<()> {
        let cwd = std::env::current_dir()?;
        let root = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(&cwd)
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map_or(cwd, |r| std::path::PathBuf::from(r.trim_end()));
        record(
            &root,
            &MergeResult {
                pr: self.pr,
                head_sha: self.head.as_deref(),
                result: &self.result,
                reason: self.reason.as_deref(),
                exit_code: self.exit_code,
            },
        );
        Ok(())
    }
}
