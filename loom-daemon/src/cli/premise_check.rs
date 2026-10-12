//! `loom-daemon premise-check` (#8396), backing `defaults/scripts/premise-check.sh`.
//!
//! The args live here rather than in `main.rs`'s `Commands` enum for the same
//! reason `DepClassifyCommand` does: `main.rs` is over
//! `.loom/docs/file-size-policy.md`'s threshold and frozen, so a new
//! subcommand must cost it nothing. What the gate answers, and why each rule
//! exists, is documented on [`loom_daemon::premise_check`].

use anyhow::Result;
use loom_daemon::observability::session_output::attended::StartRequest;
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
        if let Some(request) = self.attend_request() {
            super::attend_hook::attend("premise-check", &request);
        }
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

    /// The attended live-output request for a forge-mode gate (#10120): the
    /// first per-issue call a Curator makes, so it is where an attended
    /// Curator's output starts publishing. Local only, before any network
    /// work; it changes neither stdout nor the exit code. Hermetic mode has no
    /// issue and attends nothing.
    fn attend_request(&self) -> Option<StartRequest> {
        let issue = u32::try_from(self.issue?).ok()?;
        let workspace = self.repo_root.clone().unwrap_or_else(|| PathBuf::from("."));
        Some(super::attend_hook::request(issue, workspace))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: PremiseCheckArgs,
    }

    fn parse(argv: &[&str]) -> PremiseCheckArgs {
        Cli::try_parse_from(std::iter::once("premise-check").chain(argv.iter().copied()))
            .unwrap()
            .args
    }

    #[test]
    fn a_forge_mode_gate_attends_its_issue_under_the_agents_own_role() {
        let request = parse(&["--issue", "10120"]).attend_request().unwrap();
        assert_eq!(request.issue, 10120);
        // Hermit, Architect and Auditor run this gate too, so the role comes
        // from the subagent's `loom-<role>` type, never a hardcoded "curator".
        assert_eq!(request.role, None);
        assert_eq!(request.transcript, None);
        assert_eq!(request.workspace, PathBuf::from("."));
        let rooted = parse(&["--issue", "7", "--repo-root", "/x"])
            .attend_request()
            .unwrap();
        assert_eq!(rooted.workspace, PathBuf::from("/x"));
    }

    #[test]
    fn a_hermetic_gate_attends_nothing() {
        let args = parse(&["--body-file", "/dev/null"]);
        assert!(args.attend_request().is_none());
        assert!(parse(&["--issue=-3"]).attend_request().is_none());
    }
}
