//! `loom-daemon runtime-launch-env` (issue #8671), backing
//! `defaults/scripts/spawn-generic-launch.sh`.
//!
//! Args live here rather than in `main.rs`'s `Commands` enum for the same
//! reason every other entry in `cli::script_ports::ScriptPortCommand` does:
//! `main.rs` is over `.loom/docs/file-size-policy.md`'s threshold and
//! frozen, so a new subcommand must cost it nothing.

use anyhow::Result;
use std::path::PathBuf;

use loom_daemon::runtime_admission::EX_CONFIG;
use loom_daemon::runtime_launch::{resolve_launch_env, LaunchEnvOutcome};

#[derive(clap::Args, Debug)]
pub(crate) struct RuntimeLaunchEnvArgs {
    /// The runtime name (matches `defaults/runtimes/<name>.json`).
    #[arg(long, value_name = "NAME")]
    runtime: String,

    /// The working tree to resolve `.loom/runtimes/` /
    /// `defaults/runtimes/` against. Defaults to the invoking worktree
    /// (content resolution, not the shared-state repo root — see
    /// `repo_root::find_worktree_root`'s doc comment for why the two
    /// resolvers deliberately disagree inside a linked worktree).
    #[arg(long = "repo-root", value_name = "PATH")]
    repo_root: Option<PathBuf>,
}

impl RuntimeLaunchEnvArgs {
    /// Exit codes (documented on [`LaunchEnvOutcome`] and mirrored in
    /// `spawn-generic-launch.sh`'s own header comment):
    ///
    /// - `0` — resolved (stdout carries zero or more
    ///   `[ -n "${VAR:-}" ] || VAR="value"; export VAR` lines to `eval`).
    /// - `1` — no manifest reachable at all; soft, the caller degrades to
    ///   its legacy pure-env-var path.
    /// - `78` (`EX_CONFIG`) — a malformed manifest, or an unrecognized key
    ///   inside `launch` — fails closed rather than being silently ignored.
    pub(crate) fn run(self) -> Result<()> {
        let root = match self.repo_root {
            Some(p) => p,
            None => loom_daemon::repo_root::find_worktree_root_from_cwd()
                .unwrap_or_else(|| PathBuf::from(".")),
        };
        match resolve_launch_env(&root, &self.runtime) {
            LaunchEnvOutcome::Resolved(lines) => {
                for line in lines {
                    println!("{line}");
                }
                Ok(())
            }
            LaunchEnvOutcome::NoManifest(reason) => {
                eprintln!("runtime-launch-env: {reason}");
                std::process::exit(1);
            }
            LaunchEnvOutcome::Fatal(reason) => {
                eprintln!("runtime-launch-env: unrecognized launch shape in {reason}");
                std::process::exit(EX_CONFIG);
            }
        }
    }
}
