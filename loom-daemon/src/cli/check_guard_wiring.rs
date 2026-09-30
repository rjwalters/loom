//! `loom-daemon check-guard-wiring` — the `PreToolUse` matcher-coverage
//! contract for the `mcp__loom__.*` guard (Issue #9108).
//!
//! # Why a subcommand rather than more shell
//!
//! The check first landed as a `check_mcp_guard_wiring()` function inside
//! `scripts/check-guard-scan-contracts.sh`, which is `contract`-category shell.
//! Epic #7810's `shell-budget` gate ratchets that pool DOWN, never up, and
//! `.loom/docs/shell-language-policy.md` is unconditional: new executable logic
//! is a `loom-daemon` subcommand. This is that move — the same one
//! [`super::points_marker_check`] made for #9056, and the one this issue's own
//! guard *decision* ([`loom_daemon::mcp_tool_guard`]) made from the start.
//!
//! # Where it runs
//!
//! The `Daemon Checks` CI job, alongside `shell-budget` and the `.gitignore`
//! convergence gate — the job that already downloads the shared debug binary.
//! It deliberately does **not** live in `Structural Checks`: that job is
//! toolchain-free by design (no cargo, no daemon binary), which is exactly why
//! `check-guard-scan-contracts.sh` stays there and this does not.
//!
//! # Exit codes
//!
//! `0` = the contract holds; `1` = at least one violation, each printed to
//! stderr with the stable all-caps headline the shell original used.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::guard_wiring;

#[derive(clap::Args)]
pub(crate) struct CheckGuardWiringArgs {
    /// Repository root to check. Defaults to the current directory — CI runs
    /// it from the checkout root.
    #[arg(long, value_name = "PATH")]
    repo_root: Option<PathBuf>,
}

impl CheckGuardWiringArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = self
            .repo_root
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));

        let violations = guard_wiring::check(&root);
        if violations.is_empty() {
            println!("{}", guard_wiring::ok_message());
            return Ok(());
        }
        for v in &violations {
            eprintln!("{}", v.render());
        }
        eprintln!("{}", guard_wiring::failure_trailer());
        std::process::exit(1);
    }
}
