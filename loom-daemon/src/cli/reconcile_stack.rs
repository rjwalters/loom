//! `loom-daemon reconcile-stack` — the planning/execution half of
//! `reconcile-stack.sh` (#8583).
//!
//! # Output contract
//!
//! **stdout** is `eval`-able shell assignments and nothing else, printed only
//! after every prerequisite has held (and, with `--rebase`, after the rebase
//! has succeeded). A caller may therefore `eval` stdout unconditionally: an
//! empty stdout means "nothing was decided", never "decided, but badly".
//!
//! **stderr** carries the operator-facing diagnostics — the same `ℹ`/`⚠`
//! lines the script used to print itself.
//!
//! # Exit codes
//!
//! | code | meaning |
//! |---|---|
//! | 0 | planned (and rebased, with `--rebase`) |
//! | 1 | a prerequisite refused — **nothing was mutated** |
//! | 2 | the rebase itself failed (conflict); it is left in progress |
//!
//! 1 and 2 are kept apart because the operator's next move differs: 1 means
//! fix the precondition and re-run the whole script, 2 means resolve the
//! conflict and `git rebase --continue`. The script's own exit codes already
//! drew that line (1 = precondition, 2 = a git/gh step failed) and this
//! preserves it.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::reconcile_stack::{self, Level, PlanRequest};

#[derive(clap::Args)]
pub(crate) struct ReconcileStackArgs {
    /// The child PR's head branch (the script resolves it via `gh pr view`).
    #[arg(long, value_name = "BRANCH")]
    child_branch: String,

    /// The parent PR's head branch name. May no longer resolve — the
    /// `refs/loom/parent/<branch>` pin (#7982) is the fallback.
    #[arg(long, value_name = "BRANCH")]
    parent_branch: String,

    /// The default branch NAME. Used for the fetch and for `gh pr edit
    /// --base`; never as the rebase destination (#8583).
    #[arg(long, value_name = "BRANCH", default_value = "main")]
    default_branch: String,

    /// The remote holding the default branch.
    #[arg(long, value_name = "REMOTE", default_value = "origin")]
    remote: String,

    /// Any working tree of the repository. Defaults to the current directory.
    #[arg(long, value_name = "DIR")]
    repo_dir: Option<PathBuf>,

    /// Run the rebase. Without it this is a pure dry-run plan: it fetches and
    /// verifies every prerequisite but mutates no branch.
    #[arg(long)]
    rebase: bool,
}

impl ReconcileStackArgs {
    pub(crate) fn run(self) -> Result<()> {
        let repo_dir = match self.repo_dir {
            Some(d) => d,
            None => std::env::current_dir()?,
        };

        let req = PlanRequest {
            repo_dir: &repo_dir,
            remote: &self.remote,
            default_branch: &self.default_branch,
            child_branch: &self.child_branch,
            parent_branch: &self.parent_branch,
        };

        let plan = match reconcile_stack::plan(&req) {
            Ok(plan) => plan,
            Err(err) => {
                // Named prerequisite first: "which precondition refused" is
                // the question an operator (and a log grep) asks first.
                eprintln!(
                    "ERROR: reconcile-stack prerequisite {} failed.",
                    err.prerequisite.token()
                );
                eprintln!("{}", err.message);
                std::process::exit(1);
            }
        };

        for notice in &plan.notices {
            match notice.level {
                Level::Info => eprintln!("ℹ {}", notice.text),
                Level::Warn => eprintln!("⚠ {}", notice.text),
            }
        }

        eprintln!(
            "ℹ Step 1/3: rebase --onto {} {} {}{}",
            plan.target_commit,
            plan.parent_ref,
            plan.child_branch,
            if self.rebase {
                ""
            } else {
                "   [planned only — --rebase not given]"
            }
        );

        if self.rebase {
            if let Err(output) = reconcile_stack::rebase(&plan) {
                eprintln!("ERROR: rebase --onto failed (likely a conflict).");
                eprintln!("{output}");
                std::process::exit(2);
            }
        }

        print!("{}", reconcile_stack::render_shell(&plan));
        Ok(())
    }
}
