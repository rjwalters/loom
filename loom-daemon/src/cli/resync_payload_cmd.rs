//! `loom-daemon resync-payload` — resync a workspace's installed Loom from
//! the payload embedded in this binary, with no Loom source tree (#8961).
//!
//! Brand-new logic, native per the shell-language policy.
//! `resync-installed.sh` hands off to it when no `defaults/` source tree
//! resolves; it is also run by hand. Logic and the exit-code contract live in
//! [`loom_daemon::init::payload::standalone`].

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::init::payload::standalone;

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
        let code = match standalone::run(&dest, self.dry_run) {
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
