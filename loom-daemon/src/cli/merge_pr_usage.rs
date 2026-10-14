//! `loom-daemon merge-pr usage` (a slice of #8191).
//!
//! Prints `merge-pr.sh --help`'s text — see
//! [`loom_daemon::merge_pr::usage`] — behind the `LOOM-MERGE-PR-USAGE`
//! sentinel line. `merge-pr.sh` strips the sentinel and prints the rest; a
//! daemon predating this verb prints no sentinel (clap's error goes to
//! stderr, exit 2), so the shell falls back to a one-line usage rather than
//! printing whatever an unrelated binary said.
//!
//! # Exit code
//!
//! 0 always. There is nothing here that can fail at runtime except the
//! stdout write itself, which surfaces as an error exit and, with no
//! complete sentinel line, the same fallback.

use anyhow::Result;
use loom_daemon::merge_pr::usage::render;
use std::io::Write;

#[derive(clap::Args)]
pub(crate) struct UsageArgs {}

impl UsageArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut out = std::io::stdout().lock();
        out.write_all(render().as_bytes())?;
        out.flush()?;
        Ok(())
    }
}
