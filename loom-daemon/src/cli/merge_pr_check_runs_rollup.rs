//! `loom-daemon merge-pr check-runs-rollup` (#8191 slice): the per-poll read
//! of a check-runs rollup inside `_wait_for_checks_then_sync_merge` — see
//! [`loom_daemon::merge_pr::check_runs_rollup`].
//!
//! # Protocol
//!
//! Reads the payload `forge_get_check_runs` printed on stdin. On success,
//! exits 0 and writes four NUL-TERMINATED fields to stdout:
//!
//! ```text
//! <failing>\0<pending>\0<total_count>\0LOOM-CHECK-RUNS-ROLLUP\0
//! ```
//!
//! NUL framing because the first two fields are newline-separated name
//! lists whose names may themselves contain newlines (the retired `jq -r`
//! printed them raw); the shell reads them with `IFS= read -r -d ''`, which
//! keeps every byte. The sentinel is written LAST, so a caller that sees it
//! has necessarily seen the three fields before it.
//!
//! On a payload outside the `forge_get_check_runs` contract, exits 2 with a
//! one-line reason on stderr and NOTHING on stdout. The caller treats a
//! missing sentinel — this refusal, a missing or older binary, a crash — as
//! "still pending": the loop re-polls, and ends at its own deadline with
//! exit 5 (not merged, re-queue) if the rollup never becomes readable. It
//! never reads an unclassified rollup as settled.

use anyhow::Result;
use loom_daemon::merge_pr::check_runs_rollup::classify;
use std::io::{Read, Write};

#[derive(clap::Args)]
pub(crate) struct CheckRunsRollupArgs {}

impl CheckRunsRollupArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut raw = Vec::new();
        if let Err(e) = std::io::stdin().read_to_end(&mut raw) {
            refuse(&format!("could not read stdin: {e}"));
        }
        // Strict UTF-8: JSON text is UTF-8 by definition, and a lossy
        // replacement would invent names the forge never sent.
        let Ok(text) = String::from_utf8(raw) else {
            refuse("stdin is not valid UTF-8");
        };
        match classify(&text) {
            Ok(r) => {
                let frame = format!(
                    "{}\0{}\0{}\0LOOM-CHECK-RUNS-ROLLUP\0",
                    r.failing, r.pending, r.total_count
                );
                let mut out = std::io::stdout().lock();
                out.write_all(frame.as_bytes())?;
                out.flush()?;
                Ok(())
            }
            Err(why) => refuse(&why.to_string()),
        }
    }
}

fn refuse(why: &str) -> ! {
    eprintln!("merge-pr check-runs-rollup: refusing to classify: {why}");
    std::process::exit(2);
}
