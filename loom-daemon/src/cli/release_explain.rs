//! `loom-daemon release-explain` (#8654): the shell twin of #8515.
//!
//! `loom-daemon-update.sh`'s `fetch_resolve_latest` resolves the latest
//! release itself, and when that release carries no artifact for this host it
//! used to say only that — one flat line covering "the per-platform uploads
//! are still running" and "this platform will never be built" alike. The
//! daemon's own resolver already tells those apart
//! (`release_resolve::resolve`'s no-artifact reason); this subcommand hands
//! the shell that same classification for the release it already chose,
//! rather than growing the frozen `contract` script with a second copy.
//!
//! Contract: exit `0` with exactly one line on stdout — the classified
//! reason — when `--tag` has no artifact for `--target`; exit `1` with empty
//! stdout when it does (nothing to explain). The caller treats anything but
//! `0` + a non-empty line (including an older binary's clap "unrecognized
//! subcommand", exit `2`) as "no classification available" and keeps its own
//! flat reason, so a missing or old daemon can never fail `--fetch`.

use anyhow::Result;
use loom_daemon::release_resolve::explain_no_artifact;
use std::path::PathBuf;

#[derive(clap::Args)]
pub(crate) struct ReleaseExplainArgs {
    /// The `owner/repo` whose release is being explained.
    #[arg(long, value_name = "OWNER/NAME")]
    pub(crate) repo: String,

    /// The release tag the caller already resolved (not re-read from Latest).
    #[arg(long, value_name = "TAG")]
    pub(crate) tag: String,

    /// The release target triple the artifact was looked up for.
    #[arg(long, value_name = "TRIPLE")]
    pub(crate) target: String,

    /// Working directory for the `gh` calls. Defaults to the current
    /// directory.
    #[arg(long, value_name = "PATH")]
    pub(crate) repo_root: Option<PathBuf>,
}

impl ReleaseExplainArgs {
    /// Never returns: exits `0` with the reason, `1` when there is none.
    pub(crate) fn run(self) -> Result<()> {
        let root = self
            .repo_root
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        match explain_no_artifact(&root, &self.repo, &self.tag, &self.target) {
            Some(reason) => {
                println!("{reason}");
                std::process::exit(0);
            }
            None => std::process::exit(1),
        }
    }
}
