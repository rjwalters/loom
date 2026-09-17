//! Subcommands that back a retired shell script (epic #7810).
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

    /// Resolve the latest release artifact for this host, read-only (PR 5).
    /// Backs `loom-daemon-update.sh --resolve-json`. Exit 0 when one resolved,
    /// 1 when none did — data, not an error.
    ReleaseResolve(super::release_resolve::ReleaseResolveArgs),
}

impl ScriptPortCommand {
    /// Never returns: every arm exits with its subcommand's own code, which
    /// the stubs' callers branch on.
    pub(crate) fn run(self) -> Result<()> {
        match self {
            ScriptPortCommand::DepClassify(cmd) => cmd.run(),
            ScriptPortCommand::DepRecheckFingerprint(cmd) => cmd.run(),
            ScriptPortCommand::ReleaseResolve(args) => args.run(),
        }
    }
}
