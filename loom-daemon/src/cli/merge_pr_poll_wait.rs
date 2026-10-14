//! `loom-daemon merge-pr poll-wait` (#8191 slice): the wait-or-timeout decision
//! of `_wait_for_checks_then_sync_merge`'s unfetchable-check-runs and
//! pending-checks arms.
//!
//! # Protocol
//!
//! Reads the pending check names (one per line, `--kind pending` only) on
//! stdin. Prints exactly one line and exits 0:
//!
//! ```text
//! LOOM-POLL-WAIT <WAIT|TIMEOUT> <info|warning> <message>
//! ```
//!
//! `WAIT`: the shell narrates the message at that level and sleeps one poll
//! interval. `TIMEOUT`: the shell warns the message and exits 5 (re-queue).
//!
//! # Fail direction
//!
//! OPEN, onto the retired behaviour. A missing/older binary, a clap error or
//! silence is read by the shell as "the verb could not run": it keeps the
//! deadline comparison itself (`date +%s >= deadline`) and narrates a generic
//! line. The wait stays deadline-bounded and still exits 5; only the exact
//! wording degrades. It can never end a wait early or extend one.

use std::io::Read;

use anyhow::Result;
use loom_daemon::merge_pr::poll_wait::{decide, Inputs, Kind};

#[derive(clap::ValueEnum, Clone, Copy)]
enum KindArg {
    Unfetchable,
    Pending,
}

#[derive(clap::Args)]
pub(crate) struct PollWaitArgs {
    /// Which polling arm is asking.
    #[arg(long, value_enum)]
    kind: KindArg,

    /// The PR number (text only).
    #[arg(long, value_name = "N")]
    pr: String,

    /// The caller's clock, epoch seconds.
    #[arg(long, value_name = "SECS", allow_hyphen_values = true)]
    now: i64,

    /// The wait's deadline, epoch seconds.
    #[arg(long, value_name = "SECS", allow_hyphen_values = true)]
    deadline: i64,

    /// `LOOM_AUTO_MERGE_TIMEOUT`, interpolated verbatim.
    #[arg(long, value_name = "SECS", allow_hyphen_values = true)]
    timeout: String,

    /// `LOOM_AUTO_MERGE_POLL_INTERVAL`, interpolated verbatim.
    #[arg(long, value_name = "SECS", allow_hyphen_values = true)]
    interval: String,

    /// The failed fetch's return code (`--kind unfetchable`).
    #[arg(
        long,
        value_name = "RC",
        default_value = "0",
        allow_hyphen_values = true
    )]
    rc: String,
}

impl PollWaitArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut pending = String::new();
        if matches!(self.kind, KindArg::Pending) {
            let mut raw = Vec::new();
            std::io::stdin().read_to_end(&mut raw)?;
            pending = String::from_utf8_lossy(&raw).into_owned();
        }
        let kind = match self.kind {
            KindArg::Unfetchable => Kind::Unfetchable,
            KindArg::Pending => Kind::Pending,
        };
        let decision = decide(&Inputs {
            kind,
            pr: &self.pr,
            now: self.now,
            deadline: self.deadline,
            timeout: &self.timeout,
            interval: &self.interval,
            rc: &self.rc,
            pending: &pending,
        });
        println!("{decision}");
        Ok(())
    }
}
