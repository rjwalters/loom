//! `loom-daemon guard-hook` — the decisions behind the Bash/Write `PreToolUse`
//! guard hooks (issue #10335). The logic is [`loom_daemon::guard_hook`]; the
//! hooks (`guard-loom-workflow.sh`, `guard-worktree-paths.sh`,
//! `guard-destructive.sh`) keep only a one-line call-site, so the
//! `shell-budget` gate's hook-entry pool does not grow.
//!
//! Both subcommands are written so a missing or older daemon degrades to the
//! guard's pre-#10335 behaviour: `opted-out` signals "guards OFF" only with
//! exit 0, which a missing binary (127) or an unknown subcommand (2) never
//! produces; `mask-gh-body-heredocs` exits non-zero on any failure, and the
//! caller then scans the unmasked command.

use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::guard_hook;

#[derive(clap::Subcommand)]
pub(crate) enum GuardHookCommand {
    /// Exit 0 when the guards are explicitly opted out for `--root`
    /// (`LOOM_GUARDS_ENABLED=0|false|no`, or boolean `guards.enabled: false`
    /// in the effective config); exit 1 otherwise. Prints nothing.
    OptedOut(OptedOutArgs),

    /// Read a shell command on stdin and print it with the bodies of quoted
    /// heredocs fed to `gh issue|pr create|comment|edit` blanked. Exit 2 when
    /// stdin is not readable UTF-8.
    MaskGhBodyHeredocs,
}

#[derive(clap::Args)]
pub(crate) struct OptedOutArgs {
    /// Repository root whose config to resolve. Empty = consult only the
    /// `LOOM_GUARDS_ENABLED` env var.
    #[arg(long, value_name = "PATH", default_value = "")]
    root: PathBuf,
}

impl GuardHookCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            GuardHookCommand::OptedOut(args) => {
                std::process::exit(i32::from(!guard_hook::opted_out(&args.root)))
            }
            GuardHookCommand::MaskGhBodyHeredocs => {
                let mut raw = Vec::new();
                let text = match std::io::stdin().read_to_end(&mut raw) {
                    Ok(_) => String::from_utf8(raw).ok(),
                    Err(_) => None,
                };
                let Some(text) = text else {
                    std::process::exit(2)
                };
                let mut out = std::io::stdout().lock();
                if out
                    .write_all(guard_hook::mask_gh_body_heredocs(&text).as_bytes())
                    .and_then(|()| out.flush())
                    .is_err()
                {
                    std::process::exit(2);
                }
                Ok(())
            }
        }
    }
}
