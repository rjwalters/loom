//! `loom-daemon worktree-check` — `worktree.sh`'s in-worktree predicate and the
//! two decisions it gates (#8195 slice 11).
//!
//! # Exit-code contract
//!
//! Without `--porcelain` (the `--check` verb): **0** = the caller is inside a
//! linked worktree, **1** = it is in the main working directory. Both are
//! *answers*, which is why `worktree.sh`'s stub sets
//! `LOOM_SCRIPT_HELPER_MISSING_RC=2` — an unresolvable binary must not be
//! readable as either, and 2 is the code every epic-#7810 stub reserves for
//! "could not run at all".
//!
//! With `--porcelain` (the create path's auto-navigation arm): **0, always**.
//! "Not in a worktree" is the ordinary answer and prints nothing; the one fault
//! the shell reports on this path — an unresolvable common git dir — is
//! signalled by the absence of a `MAIN_WORKSPACE` record, because the shell
//! prints its own message *and* `--json` document for that case.
//!
//! # Why clap here
//!
//! Two callers, both inside `worktree.sh`, neither typed by a human — the same
//! argument `worktree-upstream` / `worktree-stale-ref` make. `--cwd` exists for
//! the tests: the answer is about a directory, and a subprocess suite that had
//! to `chdir` to vary it would be mutating process-global state.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::worktree_cli::check;

#[derive(clap::Args)]
pub(crate) struct WorktreeCheckArgs {
    /// Emit the create path's `LEVEL<TAB>text` record stream instead of the
    /// `--check` verb's human block, and exit 0 regardless of the answer.
    #[arg(long)]
    porcelain: bool,

    /// `--json` mode: suppress every message record, leaving only the
    /// `IN_WORKTREE` / `MAIN_WORKSPACE` data records. Matches the blanket
    /// `if [[ "$JSON_OUTPUT" != "true" ]]` the retired shell wrapped all of
    /// them in. Only meaningful with `--porcelain`.
    #[arg(long)]
    quiet: bool,

    /// The directory to answer about. Defaults to the process's current
    /// directory, which is what both `worktree.sh` call sites want.
    #[arg(long)]
    cwd: Option<PathBuf>,
}

impl WorktreeCheckArgs {
    /// Never returns.
    pub(crate) fn run(self) -> Result<()> {
        let cwd = match self.cwd {
            Some(dir) => dir,
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        let code = if self.porcelain {
            check::porcelain(&cwd, self.quiet)
        } else {
            check::report(&cwd)
        };
        std::process::exit(code);
    }
}
