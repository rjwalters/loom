//! `loom-daemon codex-sandbox-noop <capture>` (#10003): the verdict
//! `spawn-codex.sh` asks for on an exit-0 Codex session.
//!
//! The adapter tees Codex's stderr to a capture file. This reads that file
//! and answers one question: did the session run nothing because Codex's own
//! sandbox refused every shell command? The rule lives in
//! [`loom_daemon::codex_sandbox_noop`], next to the role runner's in-process
//! use of the same scan, rather than as a second copy in the frozen
//! `contract` script.
//!
//! Contract: exit `0` with exactly one line on stdout,
//! `shape=<…> execs=<n> denied=<n> succeeded=0`, for a no-op; exit `1` with
//! empty stdout when the session ran something, or when the capture cannot be
//! read. The caller treats anything except `0` plus a non-empty line as "not a
//! no-op" and keeps the shared classifier's verdict. That includes an older
//! binary's clap "unrecognized subcommand" (exit `2`). So a missing or old
//! daemon can only leave the pre-#10003 behaviour, never fail a launch.

use anyhow::Result;
use std::path::PathBuf;

#[derive(clap::Args)]
pub(crate) struct CodexSandboxNoopArgs {
    /// The captured Codex stderr of an exit-0 session.
    #[arg(value_name = "CAPTURE")]
    pub(crate) capture: PathBuf,
}

impl CodexSandboxNoopArgs {
    /// Never returns: exits `0` with the verdict line, `1` otherwise.
    pub(crate) fn run(self) -> Result<()> {
        let Ok(bytes) = std::fs::read(&self.capture) else {
            std::process::exit(1);
        };
        match loom_daemon::codex_sandbox_noop::scan(&String::from_utf8_lossy(&bytes)) {
            Some(noop) => {
                println!("{noop}");
                std::process::exit(0);
            }
            None => std::process::exit(1),
        }
    }
}
