//! Weekly-limit-point sampling for `loom-daemon tokens check` (Issue #8347,
//! part of #8063) — the write side of
//! [`loom_daemon::activity::weekly_point_history`].
//!
//! # Why this call site
//!
//! `tokens check` is the **only** process that already probes every
//! bootstrapped account on a cadence: the daemon's
//! [`loom_daemon::token_ranking_refresh`] loop shells out to `loom-daemon
//! tokens check --ranking --workspace <root>` every ~10 minutes by default.
//! Sampling from [`record_probe_run`], which `super::tokens`'
//! `run_probe_against_pool` calls once per completed probe, therefore rides
//! that existing tick — #8347 explicitly must not add a second poller.
//!
//! It hangs off the CLI's per-pool body rather than
//! `tokens_pool::check::run_check` itself for two reasons:
//!
//! - `run_check` is a pure probe/ranking routine with no database dependency,
//!   and its tests never touch anything but a temp pool directory; the
//!   activity DB is a host-level artifact (`LOOM_ACTIVITY_DB` /
//!   `~/.loom/activity.db`) the CLI layer already owns resolving.
//! - It sees the finished [`ProbeReport`] whichever internal path produced it
//!   — the live probe or the claude-monitor short-circuit — so both are
//!   sampled from one hook instead of one per `run_check` return.
//!
//! # Gated on `--ranking`
//!
//! Only invocations that are already *authoritative* about pool state persist
//! anything, the same rule `run_check` applies to `.ranking` and `.bad_tokens`
//! writes: `loom-daemon status`'s token snapshot and a bare `tokens check`
//! stay read-only diagnostics.
//!
//! # Known limitation: several distinct pools on one host
//!
//! The stored row is one scalar per UTC day, upserted with `MAX`, so when a
//! host holds several *genuinely different* pools (per-repo pools that do not
//! fall back to the shared one), the day keeps the largest single pool's total
//! rather than the union. This is inherent to the day-keyed shape, not to this
//! call site: the multi-workspace ranking refresher already probes each
//! workspace's pool in its own subprocess, so no single call site could see
//! the union either. The common case — every workspace resolving to the one
//! shared machine-level pool — is exact, and `account_count` travels with the
//! row so #8063 can tell a pool-size change apart from a real step change.

use loom_daemon::tokens_pool::check::ProbeReport;

/// Record today's weekly-limit-point sample from a completed probe run.
///
/// A no-op unless `ranking` is set (see the module doc) or the report measured
/// nothing — zero contributing accounts means no tokens are bootstrapped, or
/// every probe failed, and an upsert of 0 points would be harmless under `MAX`
/// but says nothing.
///
/// **Best-effort**: a database that cannot be opened or written warns on
/// stderr inside
/// [`loom_daemon::activity::record_daily_sample_best_effort`] and is
/// otherwise ignored, so neither `.ranking` nor `tokens check`'s exit code is
/// affected.
pub(crate) fn record_probe_run(ranking: bool, report: &ProbeReport) {
    use loom_daemon::activity::{record_daily_sample_best_effort, sum_account_points};

    if !ranking {
        return;
    }
    let (points, account_count) = sum_account_points(
        report
            .accounts
            .iter()
            .map(|account| (account.name.as_str(), account.s7d_utilization)),
    );
    if account_count == 0 {
        return;
    }
    record_daily_sample_best_effort(points, account_count);
}
