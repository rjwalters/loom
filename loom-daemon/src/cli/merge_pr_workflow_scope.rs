//! `loom-daemon merge-pr workflow-scope` (#10539): the pre-merge guard that
//! refuses a PR touching `.github/workflows/` when the active `gh` token lacks
//! the `workflow` scope, naming the fix instead of surfacing a raw 403.
//!
//! Exit 0 = proceed (nothing printed); exit 1 = block, with the refusal on
//! stdout. Any lookup failure exits 0: the guard fails open, like every other
//! lookup-based preflight, and the real merge path remains the arbiter. The
//! decision lives in [`loom_daemon::merge_pr::workflow_scope`].

use anyhow::Result;
use loom_daemon::merge_pr::workflow_scope::{assess, touches_workflows, Verdict, SKIP_ENV};
use loom_daemon::script_helpers::run_gh;

#[derive(clap::Args)]
pub(crate) struct WorkflowScopeArgs {
    /// `owner/repo` the PR lives in.
    #[arg(long, value_name = "NWO")]
    repo: String,

    /// The PR number.
    #[arg(long, value_name = "N")]
    pr: String,
}

impl WorkflowScopeArgs {
    pub(crate) fn run(self) -> Result<()> {
        if std::env::var(SKIP_ENV).is_ok_and(|v| v == "1") {
            return Ok(());
        }
        let cwd = std::env::current_dir()?;
        let endpoint = format!("repos/{}/pulls/{}/files", self.repo, self.pr);
        let files = run_gh(&["api", &endpoint, "--paginate", "--jq", ".[].filename"], &cwd, false);
        let Some(files) = files.ok_stdout_trimmed() else {
            return Ok(());
        };
        if !touches_workflows(&files) {
            return Ok(()); // no scope lookup at all
        }
        let user = run_gh(&["api", "-i", "user"], &cwd, false);
        let Some(out) = user.ok_output() else {
            return Ok(());
        };
        let response = String::from_utf8_lossy(&out.stdout);
        if let Verdict::Blocked(msg) = assess(&self.pr, &files, &response) {
            println!("{msg}");
            std::process::exit(1);
        }
        Ok(())
    }
}
