//! `loom-daemon stack-parent --issue <N> [--base <branch>]` (#12759): print the
//! open PR the Builder's committed work should be stacked on, or `none`.
//! Decision logic and the line format: [`loom_daemon::stack_parent`]. Exit 0 for
//! every decision; any other exit means "open flat".

use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;

#[derive(clap::Args)]
pub(crate) struct StackParentArgs {
    /// The Builder's issue number (its branch is `feature/issue-<N>`).
    #[arg(long)]
    issue: u32,

    /// The base the Builder was already given; anything but the default branch
    /// (`--depends-on` / `--auto-stack`) wins and the verb answers `none`.
    #[arg(long)]
    base: Option<String>,

    /// Worktree to run in. Defaults to the current directory.
    #[arg(long)]
    repo: Option<PathBuf>,
}

impl StackParentArgs {
    pub(crate) fn run(self) -> Result<()> {
        let dir = self.repo.unwrap_or_else(|| PathBuf::from("."));
        let out = loom_daemon::stack_parent::run(&dir, self.issue, self.base.as_deref());
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{}", out.decision.line(out.fork.as_deref()))?;
        Ok(())
    }
}
