//! `loom-daemon hooks-home` (2am#4501): print the directory whose
//! `defaults/hooks/` the sessions this release spawns run their wired
//! user-scope hooks from.
//!
//! The logic is [`loom_daemon::hooks_home`]: this release's payload,
//! unpacked once under `~/.local/share/loom-daemon/hooks-home/`, or the
//! `LOOM_HOOKS_HOME` pin. A host check uses this to probe the hooks role
//! sessions actually run, rather than the machine checkout.
//!
//! Contract: exit `0` with exactly one line on stdout, the directory.
//! When there is none (`LOOM_HOOKS_HOME` set empty, no home directory, or
//! the unpack failed) nothing is printed on stdout and it exits non-zero:
//! sessions then run hooks from the machine checkout.

use anyhow::{bail, Result};
use loom_daemon::hooks_home;

#[derive(clap::Args)]
pub(crate) struct HooksHomeArgs {}

impl HooksHomeArgs {
    pub(crate) fn run(self) -> Result<()> {
        let Some(dir) = hooks_home::current() else {
            bail!(
                "no hooks home: {} is set empty, there is no home directory, or this release's \
                 hooks could not be unpacked; sessions run hooks from the machine checkout",
                hooks_home::ENV
            );
        };
        println!("{}", dir.display());
        Ok(())
    }
}
