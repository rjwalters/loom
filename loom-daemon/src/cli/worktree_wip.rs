//! `loom-daemon worktree-wip` — the three WIP-shelving verbs behind
//! `worktree.sh` (#8195, epic #7810 slice 2): `snapshot`, `stash-push`,
//! `stash-pop`.
//!
//! # Output contract
//!
//! Inherited wholesale from the shell these replace, because role prompts
//! (`builder.md`, `builder-worktree.md`, `doctor.md`) and
//! `defaults/docs/guard-hooks.md` all name the verbs by path and callers chain
//! them with `&&`:
//!
//! - **Exit 0** = the verb did its job (including the legitimate no-ops: an
//!   empty snapshot, a push that found nothing, the pop that follows it).
//! - **Exit 1** = the verb ran and refused, with a reason on stderr. Every
//!   refusal is non-destructive by construction.
//! - `--json` moves human-readable lines to stderr so stdout is exactly one
//!   JSON document.
//!
//! # Why argv is taken raw
//!
//! Each verb re-implements the shell's own argument parsing rather than
//! deriving it from clap. The shell answers a missing or malformed target with
//! **exit 1** and a specific message; clap answers with **exit 2** and its own.
//! Exit 2 is the code the stub reserves for `LOOM_SCRIPT_HELPER_MISSING_RC` —
//! "no binary could be resolved" — and collapsing "you typed the target wrong"
//! into "the tool is not installed" is precisely the confusion that reservation
//! exists to prevent. `allow_hyphen_values` + `trailing_var_arg` hands the
//! whole tail through untouched so each verb's parser sees what bash's `case`
//! saw.

use anyhow::Result;

use loom_daemon::worktree_cli::{baseline, snapshot};

/// The raw tail of argv, parsed by the verb rather than by clap.
#[derive(clap::Args)]
pub(crate) struct WipArgs {
    /// `<issue-number>` (or `main`, for the stash verbs) plus any flags.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    args: Vec<String>,
}

#[derive(clap::Subcommand)]
pub(crate) enum WorktreeWipCommand {
    /// Capture a worktree's uncommitted WIP as a patch file under
    /// `<worktree-root>/.snapshots/`, without touching `git stash`.
    Snapshot(WipArgs),

    /// Capture WIP to a per-target ref and reset the tree to a clean HEAD
    /// baseline. Never writes `refs/stash`.
    StashPush(WipArgs),

    /// Restore exactly what the matching `stash-push` captured, then clear it.
    StashPop(WipArgs),
}

impl WorktreeWipCommand {
    /// Never returns: each verb exits with the code its shell predecessor did,
    /// which `&&` chains in role prompts branch on.
    pub(crate) fn run(self) -> Result<()> {
        let code = match self {
            WorktreeWipCommand::Snapshot(a) => snapshot::run(&a.args),
            WorktreeWipCommand::StashPush(a) => baseline::push(&a.args),
            WorktreeWipCommand::StashPop(a) => baseline::pop(&a.args),
        };
        std::process::exit(code);
    }
}
