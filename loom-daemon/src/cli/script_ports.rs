//! Epic #7810's subcommands: those that back a retired shell script, plus the
//! epic's own instrumentation (`shell-budget`).
//!
//! Every variant here is the implementation behind a `defaults/scripts/*.sh`
//! entry point that is now a thin stub. The stubs' names, flags, stdout and
//! exit codes are contract — role prompts invoke them by path and parse or
//! `eval` their output — so these subcommands inherit that contract.
//!
//! They are gathered into one flattened enum for two reasons. They are one
//! family, added and reviewed together as the epic lands each port; and
//! `main.rs` is over `.loom/docs/file-size-policy.md`'s threshold and frozen,
//! so each new port must cost it nothing. Flattening keeps every subcommand
//! top-level on the CLI (`loom-daemon classify-dependency-block`, not
//! `loom-daemon script-ports classify-dependency-block`) while `main.rs` holds
//! a single variant and a single dispatch arm for all of them.

use anyhow::Result;

#[derive(clap::Subcommand)]
pub(crate) enum ScriptPortCommand {
    /// Champion's dependency-classification family (PR 3):
    /// `classify-dependency-block`, `detect-dependency-cycle`,
    /// `detect-startable-subset`.
    #[command(flatten)]
    DepClassify(super::dep_classify::DepClassifyCommand),

    /// Curator's re-check fingerprints (PR 4), backing
    /// `dep-recheck-fingerprint.sh`.
    #[command(subcommand)]
    DepRecheckFingerprint(super::dep_recheck::DepRecheckCommand),

    /// Download + verify one release artifact (PR 6a). Backs
    /// `loom-daemon-update.sh`'s `fetch_and_verify_artifact`. Exit 0 verified,
    /// 1 verification failed (tamper evidence), 2 could not even download —
    /// see `cli/release_fetch.rs` for the full contract.
    ReleaseFetch(super::release_fetch::ReleaseFetchArgs),

    /// Resolve the latest release artifact for this host, read-only (PR 5).
    /// Backs `loom-daemon-update.sh --resolve-json`. Exit 0 when one resolved,
    /// 1 when none did — data, not an error.
    ReleaseResolve(super::release_resolve::ReleaseResolveArgs),

    /// How far the epic actually is: portable shell remaining, the permanent
    /// floor, and the net change since the first port. Not a port itself — it
    /// lives here because `main.rs` is frozen by the file-size ratchet and this
    /// flattened enum is what keeps a new top-level subcommand free.
    ShellBudget(super::shell_budget::ShellBudgetArgs),

    /// `worktree.sh`'s repo-global worktree-add lock (#8195, slice 1) — the
    /// lock every destructive path in that script stands behind, and which
    /// #6014/#6017 showed could be released by a holder that no longer owned
    /// it.
    #[command(subcommand)]
    WorktreeLock(super::worktree_lock::WorktreeLockCommand),

    /// `claude-wrapper.sh`'s retry/rotation classifiers (#8037): retry vs give
    /// up, rotate, mark a credential dead, and the backoff curve. Exit 0 when
    /// the predicate holds, 1 when it does not — an answer, not an error.
    #[command(subcommand)]
    RetryClassify(super::retry_classify::RetryClassifyCommand),
}

impl ScriptPortCommand {
    /// Never returns: every arm exits with its subcommand's own code, which
    /// the stubs' callers branch on.
    pub(crate) fn run(self) -> Result<()> {
        match self {
            ScriptPortCommand::DepClassify(cmd) => cmd.run(),
            ScriptPortCommand::DepRecheckFingerprint(cmd) => cmd.run(),
            ScriptPortCommand::ReleaseFetch(args) => args.run(),
            ScriptPortCommand::ReleaseResolve(args) => args.run(),
            ScriptPortCommand::ShellBudget(args) => args.run(),
            ScriptPortCommand::WorktreeLock(cmd) => cmd.run(),
            ScriptPortCommand::RetryClassify(cmd) => cmd.run(),
        }
    }
}
