//! `loom-daemon resync-payload` — resync a workspace's installed Loom from
//! the payload embedded in this binary, with no Loom source tree (#8961).
//!
//! Brand-new logic, native per the shell-language policy.
//! `resync-installed.sh` hands off to it when no `defaults/` source tree
//! resolves; it is also run by hand. Logic and the exit-code contract live in
//! [`loom_daemon::init::payload::standalone`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use loom_daemon::init::payload::standalone;
use loom_daemon::release_fetch::source;
use loom_daemon::release_provenance::{self, Provenance};

#[derive(clap::Args)]
pub(crate) struct ResyncPayloadArgs {
    /// Print what would change and write nothing. Exit 2 when something
    /// would, 0 when the install already matches.
    #[arg(long, short = 'n')]
    dry_run: bool,
    /// Repository root to resync. Defaults to the current directory.
    #[arg(long, value_name = "DIR")]
    workspace: Option<PathBuf>,
}

impl ResyncPayloadArgs {
    pub(crate) fn run(self) -> Result<()> {
        let dest = match self.workspace {
            Some(dir) => dir,
            None => std::env::current_dir()?,
        };
        let code = match standalone::run(&dest, self.dry_run, || establish_provenance(&dest)) {
            Ok(report) => {
                let text = report.render(&dest);
                if report.refused() {
                    eprint!("{text}");
                } else {
                    print!("{text}");
                }
                report.exit_code()
            }
            Err(e) => {
                eprintln!("resync-payload: {}: failed: {e:#}", dest.display());
                standalone::EXIT_FAILED
            }
        };
        std::process::exit(code);
    }
}

/// Ask the forge what this binary's release tag names: `gh api` run from
/// `dest`, peeling an annotated tag to its commit. A zero interval because a
/// one-shot process has no earlier attempt to back off from.
fn establish_provenance(dest: &Path) -> Provenance {
    release_provenance::ensure(chrono::Utc::now(), Duration::ZERO, &|repo, tag| {
        source::resolve_tag_commit(&|path| source::gh_api(dest, path), repo, tag)
    })
}
