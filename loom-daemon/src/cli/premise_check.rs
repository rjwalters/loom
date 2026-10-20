//! `loom-daemon premise-check` (#8396), backing `defaults/scripts/premise-check.sh`.
//!
//! The args live here rather than in `main.rs`'s `Commands` enum for the same
//! reason `DepClassifyCommand` does: `main.rs` is over
//! `.loom/docs/file-size-policy.md`'s threshold and frozen, so a new
//! subcommand must cost it nothing. What the gate answers, and why each rule
//! exists, is documented on [`loom_daemon::premise_check`].

use anyhow::Result;
use loom_daemon::premise_check::cli;
use std::path::PathBuf;

#[derive(clap::Args, Debug)]
pub(crate) struct PremiseCheckArgs {
    /// The issue to gate (forge mode).
    #[arg(long, value_name = "N")]
    issue: Option<i64>,

    /// Defaults to the checkout's origin remote.
    #[arg(long, value_name = "OWNER/REPO")]
    repo: Option<String>,

    /// Read the issue body from a file instead of the forge (hermetic mode).
    #[arg(long = "body-file", value_name = "PATH", conflicts_with = "issue")]
    body_file: Option<PathBuf>,

    /// Issue title, hermetic mode only.
    #[arg(long, value_name = "TEXT", requires = "body_file")]
    title: Option<String>,

    /// Comma-separated label names, hermetic mode only.
    #[arg(long, value_name = "A,B", requires = "body_file")]
    labels: Option<String>,

    /// Extra text searched for the premise record (stands in for the issue's
    /// comments), hermetic mode only.
    #[arg(long = "record-file", value_name = "PATH", requires = "body_file")]
    record_file: Option<PathBuf>,

    /// Root the evidence scan and every citation resolves against. Defaults to
    /// the WORKING TREE containing the cwd — a linked worktree resolves
    /// against itself, not the main checkout it was created from (#8499).
    #[arg(long = "repo-root", value_name = "PATH")]
    repo_root: Option<PathBuf>,

    /// Skip the advisory evidence scan (it is two `git grep` passes).
    #[arg(long = "no-scan")]
    no_scan: bool,

    /// Most evidence candidates to print.
    #[arg(long = "scan-limit", value_name = "N", default_value_t = 8)]
    scan_limit: usize,

    /// Bypass the `gh` read cache.
    #[arg(long = "no-cache")]
    no_cache: bool,
}

impl PremiseCheckArgs {
    /// Never returns: exits with the gate's own code, which role prompts and
    /// the sweep orchestrator branch on.
    pub(crate) fn run(self) -> Result<()> {
        cli::run(&cli::Options {
            issue: self.issue,
            repo: self.repo,
            body_file: self.body_file,
            title: self.title,
            labels: self.labels,
            record_file: self.record_file,
            repo_root: self.repo_root,
            no_scan: self.no_scan,
            scan_limit: self.scan_limit,
            no_cache: self.no_cache,
        })
    }
}
