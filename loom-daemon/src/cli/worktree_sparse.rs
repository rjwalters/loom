//! `loom-daemon worktree-sparse` — `worktree.sh`'s `--sparse <paths...>` /
//! `--full` family (#8195 slice 10, epic #7810).
//!
//! # Exit-code contract
//!
//! 0 applied; 1 refused or failed (not a registered worktree, a cone git
//! rejects, an unwritable sentinel); 2 could not run (clap's own usage error,
//! or `--full` with `--arm create`). See [`worktree_cli::sparse`]'s module
//! docs for why 1 replaces the retired script's silent 128.
//!
//! # Why the cone paths come after `--`
//!
//! `worktree.sh` collects `--sparse` paths up to the next `--flag`, so a path
//! may begin with a single `-`. As a trailing `last = true` positional they are
//! taken verbatim, never parsed as options.
//!
//! # Why clap here, like `worktree-upstream`
//!
//! The callers are generated command lines inside `worktree.sh`; no human
//! types them, so clap's usage error is the right answer to a malformed one.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::worktree_cli::sparse;

/// Which retired block this invocation is. Kebab-case on the command line.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ArmArg {
    /// Right after `git worktree add --no-checkout` created the worktree.
    Create,
    /// The worktree directory already existed; apply the mode and conclude.
    Reconfigure,
}

fn issue_number(s: &str) -> std::result::Result<String, String> {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        Ok(s.to_string())
    } else {
        Err(format!("issue number must be numeric (got: '{s}')"))
    }
}

#[derive(clap::Args)]
pub(crate) struct WorktreeSparseArgs {
    /// Which retired block this invocation is.
    #[arg(long, value_enum)]
    arm: ArmArg,

    /// `$WORKTREE_PATH` (re-configure) or `$ABS_WORKTREE_PATH` (create).
    #[arg(long)]
    worktree: PathBuf,

    /// `$ISSUE_NUMBER` — written into the sentinel and the JSON document, and
    /// quoted in the create arm's recovery hint. Digits only: it is spliced
    /// into JSON unquoted, as the retired document did.
    #[arg(long, value_parser = issue_number)]
    issue: String,

    /// `$BRANCH_NAME` — written into the sentinel and the JSON document.
    #[arg(long)]
    branch: String,

    /// Disable sparse-checkout instead of applying a cone.
    #[arg(long, conflicts_with = "paths")]
    full: bool,

    /// Machine-readable mode: no narration. `reconfigure` prints its JSON
    /// document; `create` prints the cone array for the caller's.
    #[arg(long)]
    json: bool,

    /// The caller's `--sparse` paths, before the always-included set.
    #[arg(last = true)]
    paths: Vec<OsString>,
}

impl WorktreeSparseArgs {
    /// Never returns.
    pub(crate) fn run(self) -> Result<()> {
        std::process::exit(sparse::run(&sparse::Options {
            repo: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            worktree: self.worktree,
            arm: match self.arm {
                ArmArg::Create => sparse::Arm::Create,
                ArmArg::Reconfigure => sparse::Arm::Reconfigure,
            },
            mode: if self.full {
                sparse::Mode::Full
            } else {
                sparse::Mode::Sparse(self.paths)
            },
            issue: self.issue,
            branch: self.branch,
            json: self.json,
            extra_include: std::env::var_os("LOOM_WORKTREE_ALWAYS_INCLUDE"),
        }));
    }
}
