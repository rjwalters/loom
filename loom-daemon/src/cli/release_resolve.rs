//! `loom-daemon release resolve` (epic #7810, PR 5).
//!
//! Backs `loom-daemon-update.sh --resolve-json`, which delegates here rather
//! than building the object a second time. Its 27 assertions in
//! `test-loom-daemon-update.sh` were written against the shell and now drive
//! this, unchanged — the equivalence proof.
//!
//! Exit `0` when an artifact resolved, `1` when none did. `1` is **data**, not
//! an error: the daemon's tick falls back to its source path on it.

use anyhow::Result;
use loom_daemon::release_resolve::{emit, resolve, Inputs};
use std::path::PathBuf;

#[derive(clap::Args)]
pub(crate) struct ReleaseResolveArgs {
    /// The checkout whose `origin` remote names the repo, and whose `VERSION`
    /// and `HEAD` are reported. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub(crate) repo_root: Option<PathBuf>,

    /// The binary to report as installed. Caller-supplied because "installed"
    /// differs per caller — see `Inputs::installed_bin`.
    #[arg(long, value_name = "PATH")]
    pub(crate) installed_bin: Option<PathBuf>,

    /// Override the detected release target triple
    /// (`LOOM_DAEMON_UPDATE_TARGET`).
    #[arg(long, value_name = "TRIPLE")]
    pub(crate) target: Option<String>,

    /// Override the detected `owner/repo` (`LOOM_DAEMON_UPDATE_GH_REPO`).
    #[arg(long, value_name = "OWNER/NAME")]
    pub(crate) repo: Option<String>,

    /// Report `ok:false` with the fetch-disabled reason without asking the
    /// forge anything (`--no-fetch` / `LOOM_DAEMON_UPDATE_FETCH=0`).
    #[arg(long = "no-fetch")]
    pub(crate) no_fetch: bool,
}

impl ReleaseResolveArgs {
    /// Never returns: exits `0` when an artifact resolved, `1` when none did.
    pub(crate) fn run(self) -> Result<()> {
        let root = self
            .repo_root
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        // Env vars are read here rather than inside `resolve` so the library
        // half stays a pure function of its inputs — a flag and an env var are
        // both just a caller's opinion.
        let inputs = Inputs {
            repo_root: &root,
            target_override: self
                .target
                .or_else(|| std::env::var("LOOM_DAEMON_UPDATE_TARGET").ok()),
            repo_override: self
                .repo
                .or_else(|| std::env::var("LOOM_DAEMON_UPDATE_GH_REPO").ok()),
            installed_bin: self.installed_bin,
            fetch_disabled: self.no_fetch
                || std::env::var("LOOM_DAEMON_UPDATE_FETCH")
                    .map(|v| matches!(v.trim(), "0" | "false" | "no" | "off"))
                    .unwrap_or(false),
        };

        let resolution = resolve(&inputs);
        // Exactly one line on stdout, and nothing else ever written there.
        println!("{}", emit::to_json(&resolution));
        std::process::exit(match resolution {
            loom_daemon::release_resolve::Resolution::Resolved(_) => 0,
            loom_daemon::release_resolve::Resolution::Unresolved(_) => 1,
        });
    }
}
