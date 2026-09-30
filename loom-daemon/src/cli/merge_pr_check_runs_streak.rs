//! `loom-daemon merge-pr check-runs-streak` (#6389, an #8191 slice): the
//! persistent-vs-transient check-runs HTTP 404 classification inside
//! `_wait_for_checks_then_sync_merge`'s poll loop.
//!
//! # Protocol
//!
//! Invoked ONLY once a poll's fetch attempt has already failed (the shell
//! establishes `fetch_rc != 0` before calling this — see the module docs on
//! [`loom_daemon::merge_pr::check_runs_streak`] for why a successful fetch
//! never reaches here). Always exits 0 with exactly one line:
//!
//! ```text
//! LOOM-CHECK-RUNS-STREAK <PROCEED|PENDING> <new-streak>
//! ```
//!
//! | verdict | the shell does |
//! |---|---|
//! | `PROCEED` | log the "check-runs API unavailable" info line and `return 0` (proceed to the synchronous merge) |
//! | `PENDING` | fall through unchanged to the existing truncated-check / deadline / sleep-and-continue handling |
//!
//! Anything else on stdout — a missing/older binary, a clap usage error,
//! silence — must be read by the caller as "the classification could not run"
//! and treated as `PENDING` with the streak reset to zero: the pre-#6389
//! behaviour. That degraded mode can only ever cost time (bounded by the
//! caller's own `LOOM_AUTO_MERGE_TIMEOUT`), never misclassify a transient
//! blip as the persistent condition that skips waiting altogether.

use anyhow::Result;
use loom_daemon::merge_pr::check_runs_streak::{decide, render, Inputs};

#[derive(clap::Args)]
pub(crate) struct CheckRunsStreakArgs {
    /// The first fetch attempt's return code this iteration.
    #[arg(long, value_name = "RC")]
    attempt1_rc: i32,

    /// The retry's return code (meaningful only when `--attempt1-rc` is
    /// nonzero, mirroring the shell's own retry-once absorption).
    #[arg(long, value_name = "RC")]
    attempt2_rc: i32,

    /// The running streak BEFORE this iteration (the caller's cached
    /// `not_found_streak`).
    #[arg(long, value_name = "N", default_value_t = 0)]
    streak: u64,

    /// `LOOM_CHECK_RUNS_404_STREAK` — consecutive confirmed 404s required
    /// before giving up on the wait.
    #[arg(long, value_name = "N", default_value_t = 2)]
    threshold: u64,

    /// `FORGE_CHECK_RUNS_RC_NOT_FOUND` (lib/forge-helpers.sh's dedicated
    /// confirmed-404 return code), passed in so this binary never hardcodes a
    /// second copy of that constant.
    #[arg(long, value_name = "RC", default_value_t = 44)]
    not_found_rc: i32,
}

impl CheckRunsStreakArgs {
    pub(crate) fn run(self) -> Result<()> {
        let decision = decide(&Inputs {
            attempt1_rc: self.attempt1_rc,
            attempt2_rc: self.attempt2_rc,
            streak_in: self.streak,
            threshold: self.threshold,
            not_found_rc: self.not_found_rc,
        });
        println!("{}", render(&decision));
        Ok(())
    }
}
