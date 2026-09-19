//! `loom-daemon worktree-state` — say what an agent actually left behind
//! (Issue #8267).
//!
//! # Output contract
//!
//! - `report` → one `key=value` line on stdout (or `--json`), exit **0** when
//!   nothing is at risk, exit **3** when deliverable-shaped changes are sitting
//!   uncommitted. `3` rather than `1` for the same reason
//!   `check-main-clean.sh` uses it: a caller must be able to tell "there is
//!   unsaved work here" from "the command itself failed".
//! - `stop-hook` → the `Stop`/`SubagentStop` hook JSON protocol: a decision
//!   object on stdout, **always exit 0**. A hook that exits non-zero on its own
//!   bug would surface as a broken session rather than as a missed check.
//!
//! The logic lives in [`loom_daemon::worktree_state`]; this module is argument
//! parsing and exit codes only.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::worktree_state::{self, stop_hook};

/// The base ref every Loom issue worktree branches from. Overridable for a
/// repo whose default branch is not `main`, and for tests.
const DEFAULT_BASE_REF: &str = "origin/main";

#[derive(clap::Subcommand)]
pub(crate) enum WorktreeStateCommand {
    /// Print one worktree's branch/worktree state. Exit 3 when work is unsaved.
    Report(ReportArgs),

    /// Evaluate a `Stop`/`SubagentStop` hook payload read from stdin and print
    /// the hook's decision JSON. Always exits 0.
    StopHook(StopHookArgs),
}

impl WorktreeStateCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            WorktreeStateCommand::Report(args) => args.run(),
            WorktreeStateCommand::StopHook(args) => args.run(),
        }
    }
}

#[derive(clap::Args)]
pub(crate) struct ReportArgs {
    /// Worktree to measure. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    worktree: Option<PathBuf>,

    /// Measure `<repo>/.loom/worktrees/issue-<N>` instead of `--worktree`.
    #[arg(long, value_name = "N", conflicts_with = "worktree")]
    issue: Option<u32>,

    /// Repo root used to resolve `--issue`. Defaults to the current directory's
    /// main checkout.
    #[arg(long, value_name = "PATH")]
    repo_root: Option<PathBuf>,

    /// Branch point to count commits against.
    #[arg(long, value_name = "REF", default_value = DEFAULT_BASE_REF)]
    base: String,

    /// Emit the full record as JSON instead of the one-line form.
    #[arg(long)]
    json: bool,

    /// Measure and set the exit code, but print nothing.
    #[arg(long)]
    quiet: bool,
}

impl ReportArgs {
    fn run(self) -> Result<()> {
        let worktree = match (self.worktree, self.issue) {
            (Some(path), _) => path,
            (None, Some(issue)) => {
                let root = match self.repo_root {
                    Some(root) => root,
                    None => std::env::current_dir()?,
                };
                loom_daemon::worktree_root::worktree_root(&root).join(format!("issue-{issue}"))
            }
            (None, None) => std::env::current_dir()?,
        };

        if !worktree.is_dir() {
            // Not an error worth a non-zero "unsaved work" code — there is
            // provably nothing at risk in a directory that does not exist.
            eprintln!("worktree-state: no such worktree: {}", worktree.display());
            std::process::exit(0);
        }

        let state = worktree_state::collect(&worktree, &self.base);
        if !self.quiet {
            if self.json {
                println!("{}", serde_json::to_string_pretty(&state.to_json())?);
            } else {
                println!("{}", state.render_line());
            }
        }
        std::process::exit(state.verdict.exit_code());
    }
}

#[derive(clap::Args)]
pub(crate) struct StopHookArgs {
    /// Branch point to count commits against.
    #[arg(long, value_name = "REF", default_value = DEFAULT_BASE_REF)]
    base: String,
}

impl StopHookArgs {
    fn run(self) -> Result<()> {
        let mut raw = String::new();
        // Every failure below is an ALLOW: a guard that cannot read its own
        // payload must not be the reason a session cannot end.
        if std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw).is_err() {
            std::process::exit(0);
        }
        let payload: stop_hook::HookPayload = serde_json::from_str(&raw).unwrap_or_default();
        let decision = stop_hook::evaluate(&payload, &self.base);
        if let Some(json) = decision.to_hook_json() {
            println!("{json}");
        }
        std::process::exit(0);
    }
}
