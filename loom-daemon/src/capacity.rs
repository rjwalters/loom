//! Token-capacity backpressure + add-capacity advisory for the autonomous work
//! finder (Issue #3902, epic #3809).
//!
//! The work finder (#3810) drives approved `loom:issue` work to dispatch, and
//! #3811 bounds its concurrency by `min(token-pool size, disk headroom,
//! configured max)`. That policy treats the token pool as a flat count of
//! `*.token` files — but at scale accounts hit their 5h/7d rate limits and go
//! **exhausted**. Dispatching to an exhausted account produces the startup
//! hangs / mid-build deaths seen while dogfooding. This module makes the
//! daemon treat a genuine token limit as a **capacity signal**:
//!
//! 1. **Slow down** — [`token_axis_limit`] backs the token axis off from the
//!    raw pool size toward the count of *healthy* (`available`) accounts, read
//!    from the rotation ranking file (`.loom/tokens/.ranking`). When every
//!    account is exhausted the limit drops to 0 and the finder defers (never
//!    hammers an exhausted account). A single healthy account is the throughput
//!    floor, never a halt (operator refinement on #3902).
//! 2. **Alert** — [`assess_pressure`] derives whether the token axis is the
//!    binding constraint *and* work is queued behind it, and
//!    [`CapacityAdvisory::message`] builds an operator advisory naming the
//!    concrete levers (add accounts + `loom-tokens bootstrap`, or buy API
//!    credits, then `loom-tokens check --ranking`). The advisory surfaces on the
//!    daemon status view, the event bus (`daemon.capacity.advisory`), and the
//!    log — deduplicated to fire only on **state change**.
//! 3. **Recover** — the finder re-reads the ranking every tick (bounded cadence
//!    = the tick interval), so as accounts reset to `available` the limit ramps
//!    back up and the backlog drains automatically, with a symmetric recovery
//!    event/log line.
//!
//! # Why the ranking file (not a network probe)
//!
//! The daemon never performs the slow per-account rate-limit probe itself — that
//! is `loom-tokens check`'s job, run out-of-band (cron / `probe-tokens.sh`),
//! which writes the discrete status into `.loom/tokens/.ranking` (the
//! format-of-record the spawn-time selector already consumes). Reading that file
//! keeps this module a fast, non-blocking, filesystem-only read that matches the
//! footing of [`crate::tokens::token_pool_size`] and
//! [`crate::disk_headroom::disk_headroom_limit`].
//!
//! # Near-ceiling granularity
//!
//! The `.ranking` format carries only the discrete status word
//! (`available` / `exhausted` / `rate_limited` / `blocked`), where `exhausted`
//! is already assigned by the probe at 7d utilization ≥ 0.95. A finer
//! "near-ceiling ≥ 0.90 but not yet exhausted" bucket would require the richer
//! per-account utilization JSON (`loom-tokens check --json`); this module treats
//! any non-`available` status as unhealthy, which is the actionable signal for
//! backpressure (do not dispatch to it). Sub-`exhausted` utilization thresholds
//! are a documented follow-up.

use std::path::Path;

// ============================================================================
// Constants
// ============================================================================

/// Minimum count of token-bound queued (deferred) issues before the daemon
/// enters the *pressured* state and fires the add-capacity advisory. One is the
/// smallest actionable signal (approved work exists that the healthy accounts
/// cannot take right now); the state-change dedup keeps it from spamming.
pub const DEFAULT_ADVISORY_MIN_QUEUED: usize = 1;

/// Nominal per-sweep wall-clock minutes used to estimate backlog drain time.
///
/// The daemon does not track a live per-sweep duration here, so the drain
/// estimate is deliberately a coarse "at current healthy capacity" figure:
/// `ceil(queued / healthy) * NOMINAL_SWEEP_MINUTES`. It is an order-of-magnitude
/// aid for the operator ("~hours, not ~minutes"), not a precise SLA.
pub const NOMINAL_SWEEP_MINUTES: u64 = 30;

// ============================================================================
// Ranking snapshot
// ============================================================================

/// Health classification of a single rotation account, parsed from the discrete
/// status word in `.loom/tokens/.ranking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountHealth {
    /// `available` — under the rate-limit ceiling; safe to dispatch to.
    Available,
    /// `exhausted` — 7d utilization ≥ 0.95 per the probe; do not dispatch.
    Exhausted,
    /// `rate_limited` — a current 429; do not dispatch.
    RateLimited,
    /// `blocked` — 401 auth failure or listed in `.bad_tokens`; do not dispatch.
    Blocked,
    /// Any other/unrecognized status word — treated as unhealthy (fail safe:
    /// do not dispatch to an account whose health we cannot confirm).
    Unknown,
}

impl AccountHealth {
    /// Parse a `.ranking` status word. Unrecognized words map to
    /// [`AccountHealth::Unknown`] (unhealthy) rather than silently counting as
    /// available.
    #[must_use]
    pub fn parse(status: &str) -> Self {
        match status.trim() {
            "available" => Self::Available,
            "exhausted" => Self::Exhausted,
            "rate_limited" => Self::RateLimited,
            "blocked" => Self::Blocked,
            _ => Self::Unknown,
        }
    }

    /// True only for [`AccountHealth::Available`] — the sole state safe to
    /// dispatch a new sweep to.
    #[must_use]
    pub fn is_healthy(self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Aggregate account-health counts derived from `.loom/tokens/.ranking`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RankingSnapshot {
    /// Total accounts listed in the ranking file.
    pub total: usize,
    /// Accounts with status `available` — the healthy, dispatchable set.
    pub available: usize,
    /// Accounts with status `exhausted`.
    pub exhausted: usize,
    /// Accounts with status `rate_limited`.
    pub rate_limited: usize,
    /// Accounts with status `blocked`.
    pub blocked: usize,
    /// Accounts with an unrecognized status word (counted as unhealthy).
    pub unknown: usize,
}

impl RankingSnapshot {
    /// Number of unhealthy (non-`available`) accounts — exhausted, rate-limited,
    /// blocked, or unknown.
    #[must_use]
    pub fn unhealthy(&self) -> usize {
        self.total.saturating_sub(self.available)
    }
}

/// Parse the rotation ranking file at `{workspace_root}/.loom/tokens/.ranking`.
///
/// The file is the pipe-delimited `name|status` format written by
/// `loom-tokens check --ranking` (one account per line). Returns `None` when the
/// file is absent, unreadable, or contains no parseable rows — the signal that
/// no probe data exists, in which case callers fall back to the raw token-pool
/// size (byte-for-byte the pre-#3902 behavior). Blank lines and malformed rows
/// (no `|`) are skipped; a row's status is parsed via [`AccountHealth::parse`].
#[must_use]
pub fn read_ranking(workspace_root: &Path) -> Option<RankingSnapshot> {
    let ranking_path = workspace_root.join(".loom").join("tokens").join(".ranking");
    let contents = std::fs::read_to_string(&ranking_path).ok()?;
    let mut snap = RankingSnapshot::default();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // `name|status` — the status is the last pipe-delimited field so a name
        // containing a stray pipe (there are none in practice) still parses.
        let Some((_name, status)) = line.rsplit_once('|') else {
            continue;
        };
        snap.total += 1;
        match AccountHealth::parse(status) {
            AccountHealth::Available => snap.available += 1,
            AccountHealth::Exhausted => snap.exhausted += 1,
            AccountHealth::RateLimited => snap.rate_limited += 1,
            AccountHealth::Blocked => snap.blocked += 1,
            AccountHealth::Unknown => snap.unknown += 1,
        }
    }
    if snap.total == 0 {
        None
    } else {
        Some(snap)
    }
}

/// The health-adjusted token-axis concurrency limit.
///
/// When ranking data exists, this is the count of `available` accounts — the
/// finder never dispatches beyond the healthy set, so it never targets an
/// exhausted/blocked account and never over-subscribes. When ranking data is
/// absent (no probe has run), it falls back to `pool_size` — the pre-#3902
/// behavior, so a repo that has not wired up `loom-tokens check` sees zero
/// change.
#[must_use]
pub fn token_axis_limit(workspace_root: &Path, pool_size: usize) -> usize {
    match read_ranking(workspace_root) {
        Some(snap) => snap.available,
        None => pool_size,
    }
}

// ============================================================================
// Pressure assessment
// ============================================================================

/// A point-in-time capacity assessment: whether the token axis is the binding
/// constraint, how much work is queued behind it, and the derived operator
/// numbers (healthy/exhausted accounts, estimated drain time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PressureAssessment {
    /// True when the token axis is the binding (minimum) constraint on the
    /// dynamic cap **and** work was deferred this tick — i.e. the queue is held
    /// back specifically by token capacity (not disk or the operator ceiling).
    pub token_bound: bool,
    /// True when [`Self::token_bound`] holds and the queued count meets the
    /// advisory threshold — the daemon should surface an add-capacity advisory.
    pub pressured: bool,
    /// Issues deferred to a future tick because the token-bound cap was reached.
    pub queued: usize,
    /// Healthy (`available`) accounts driving current throughput.
    pub healthy_accounts: usize,
    /// Unhealthy (exhausted / rate-limited / blocked / unknown) accounts.
    pub exhausted_accounts: usize,
    /// Total accounts in the ranking (or the raw pool size when no ranking).
    pub total_accounts: usize,
    /// Estimated minutes to drain the queued backlog at current healthy
    /// capacity, or `None` when no healthy account exists (cannot drain until
    /// capacity is restored).
    pub estimated_drain_minutes: Option<u64>,
}

/// Derive a [`PressureAssessment`] from the tick's live inputs.
///
/// - `ranking` — parsed ranking snapshot, or `None` when no probe data exists.
/// - `pool_size` — raw `*.token` count (the fallback health basis).
/// - `token_limit` — the health-adjusted token-axis limit
///   ([`token_axis_limit`]).
/// - `disk` / `configured_max` — the other two dynamic-cap axes.
/// - `deferred` — issues the tick deferred for capacity ([`crate`]'s
///   `TickReport::deferred_capacity`).
/// - `min_queued` — the advisory threshold ([`DEFAULT_ADVISORY_MIN_QUEUED`]).
///
/// `token_bound` requires that the token axis is the (co-)minimum of the three
/// cap axes: dispatch is held back by tokens, not by a full disk or the operator
/// ceiling. This keeps the advisory from firing when the real bottleneck is disk
/// or a deliberately low `maxConcurrent`.
#[must_use]
pub fn assess_pressure(
    ranking: Option<&RankingSnapshot>,
    pool_size: usize,
    token_limit: usize,
    disk: usize,
    configured_max: usize,
    deferred: usize,
    min_queued: usize,
) -> PressureAssessment {
    let token_bound = deferred > 0 && token_limit <= disk && token_limit <= configured_max;
    let pressured = token_bound && deferred >= min_queued;

    let (total_accounts, healthy_accounts, exhausted_accounts) = match ranking {
        Some(r) => (r.total, r.available, r.unhealthy()),
        // No ranking: treat the whole pool as healthy (unknown = optimistic
        // fallback, matching the token_axis_limit fallback).
        None => (pool_size, pool_size, 0),
    };

    let estimated_drain_minutes = if healthy_accounts == 0 {
        None
    } else {
        // ceil(queued / healthy) waves, each a nominal sweep duration.
        let waves = deferred.div_ceil(healthy_accounts);
        Some(waves as u64 * NOMINAL_SWEEP_MINUTES)
    };

    PressureAssessment {
        token_bound,
        pressured,
        queued: deferred,
        healthy_accounts,
        exhausted_accounts,
        total_accounts,
        estimated_drain_minutes,
    }
}

// ============================================================================
// Advisory rendering
// ============================================================================

/// A rendered add-capacity advisory (or its symmetric recovery counterpart),
/// carrying the numbers plus a human-readable message that names the concrete
/// levers. Consumed by the work-finder loop to emit the log line and the
/// `daemon.capacity.advisory` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityAdvisory {
    /// True when entering the pressured state; false on recovery.
    pub pressured: bool,
    /// Queued (deferred) issue count at the transition.
    pub queued: usize,
    /// Healthy account count at the transition.
    pub healthy_accounts: usize,
    /// Unhealthy account count at the transition.
    pub exhausted_accounts: usize,
    /// Total account count at the transition.
    pub total_accounts: usize,
    /// Estimated drain minutes at the transition (`None` = no healthy capacity).
    pub estimated_drain_minutes: Option<u64>,
    /// Operator-facing one-line message.
    pub message: String,
}

impl CapacityAdvisory {
    /// Build the *entered-pressure* advisory from an assessment.
    #[must_use]
    pub fn pressure(a: &PressureAssessment) -> Self {
        let drain = a.estimated_drain_minutes.map_or_else(
            || "no healthy accounts — stalled until capacity returns".to_string(),
            format_minutes,
        );
        let message = format!(
            "token capacity: {queued} issue(s) queued; {healthy}/{total} accounts healthy, \
             {exhausted} exhausted/near-ceiling; est. ~{drain} to drain at current capacity. \
             Add accounts to ~/.claude-monitor/accounts.env then `loom-tokens bootstrap`, or buy \
             API credits, then re-probe with `loom-tokens check --ranking`.",
            queued = a.queued,
            healthy = a.healthy_accounts,
            total = a.total_accounts,
            exhausted = a.exhausted_accounts,
        );
        Self {
            pressured: true,
            queued: a.queued,
            healthy_accounts: a.healthy_accounts,
            exhausted_accounts: a.exhausted_accounts,
            total_accounts: a.total_accounts,
            estimated_drain_minutes: a.estimated_drain_minutes,
            message,
        }
    }

    /// Build the *recovered* advisory from an assessment (symmetric with
    /// [`Self::pressure`]).
    #[must_use]
    pub fn recovery(a: &PressureAssessment) -> Self {
        let message = format!(
            "token capacity restored: {healthy}/{total} accounts healthy; queued backlog \
             draining automatically — no action needed.",
            healthy = a.healthy_accounts,
            total = a.total_accounts,
        );
        Self {
            pressured: false,
            queued: a.queued,
            healthy_accounts: a.healthy_accounts,
            exhausted_accounts: a.exhausted_accounts,
            total_accounts: a.total_accounts,
            estimated_drain_minutes: a.estimated_drain_minutes,
            message,
        }
    }
}

/// Render a minutes count as a compact `~Xh Ym` / `~Ym` string.
#[must_use]
pub fn format_minutes(mins: u64) -> String {
    if mins >= 60 {
        let h = mins / 60;
        let m = mins % 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    } else {
        format!("{mins}m")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn write_ranking(workspace: &Path, body: &str) {
        let dir = workspace.join(".loom").join("tokens");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".ranking"), body).unwrap();
    }

    // ------------------------------------------------------------------
    // AccountHealth
    // ------------------------------------------------------------------

    #[test]
    fn account_health_parse_and_healthy() {
        assert_eq!(AccountHealth::parse("available"), AccountHealth::Available);
        assert_eq!(AccountHealth::parse("exhausted"), AccountHealth::Exhausted);
        assert_eq!(AccountHealth::parse("rate_limited"), AccountHealth::RateLimited);
        assert_eq!(AccountHealth::parse("blocked"), AccountHealth::Blocked);
        assert_eq!(AccountHealth::parse("weird"), AccountHealth::Unknown);
        assert_eq!(AccountHealth::parse(" available "), AccountHealth::Available);

        assert!(AccountHealth::Available.is_healthy());
        assert!(!AccountHealth::Exhausted.is_healthy());
        assert!(!AccountHealth::RateLimited.is_healthy());
        assert!(!AccountHealth::Blocked.is_healthy());
        assert!(!AccountHealth::Unknown.is_healthy());
    }

    // ------------------------------------------------------------------
    // read_ranking
    // ------------------------------------------------------------------

    #[test]
    fn read_ranking_missing_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_ranking(tmp.path()), None);
    }

    #[test]
    fn read_ranking_empty_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_ranking(tmp.path(), "\n  \n");
        assert_eq!(read_ranking(tmp.path()), None);
    }

    #[test]
    fn read_ranking_counts_statuses() {
        let tmp = tempfile::tempdir().unwrap();
        write_ranking(
            tmp.path(),
            "a|available\nb|available\nc|exhausted\nd|rate_limited\ne|blocked\nf|weird\n",
        );
        let snap = read_ranking(tmp.path()).unwrap();
        assert_eq!(snap.total, 6);
        assert_eq!(snap.available, 2);
        assert_eq!(snap.exhausted, 1);
        assert_eq!(snap.rate_limited, 1);
        assert_eq!(snap.blocked, 1);
        assert_eq!(snap.unknown, 1);
        assert_eq!(snap.unhealthy(), 4);
    }

    #[test]
    fn read_ranking_skips_malformed_rows() {
        let tmp = tempfile::tempdir().unwrap();
        write_ranking(tmp.path(), "a|available\nno-pipe-here\n\nb|exhausted\n");
        let snap = read_ranking(tmp.path()).unwrap();
        assert_eq!(snap.total, 2, "the pipe-less row is skipped");
        assert_eq!(snap.available, 1);
        assert_eq!(snap.exhausted, 1);
    }

    // ------------------------------------------------------------------
    // token_axis_limit
    // ------------------------------------------------------------------

    #[test]
    fn token_axis_limit_uses_available_when_ranking_present() {
        let tmp = tempfile::tempdir().unwrap();
        write_ranking(tmp.path(), "a|available\nb|available\nc|exhausted\n");
        // 3 token files present, but only 2 available → limit 2.
        assert_eq!(token_axis_limit(tmp.path(), 3), 2);
    }

    #[test]
    fn token_axis_limit_falls_back_to_pool_when_no_ranking() {
        let tmp = tempfile::tempdir().unwrap();
        // No ranking file → fall back to raw pool size (pre-#3902 behavior).
        assert_eq!(token_axis_limit(tmp.path(), 5), 5);
    }

    #[test]
    fn token_axis_limit_zero_when_all_exhausted() {
        let tmp = tempfile::tempdir().unwrap();
        write_ranking(tmp.path(), "a|exhausted\nb|blocked\n");
        assert_eq!(token_axis_limit(tmp.path(), 2), 0, "never dispatch to exhausted");
    }

    // ------------------------------------------------------------------
    // assess_pressure
    // ------------------------------------------------------------------

    fn snap(total: usize, available: usize) -> RankingSnapshot {
        RankingSnapshot {
            total,
            available,
            exhausted: total - available,
            ..RankingSnapshot::default()
        }
    }

    #[test]
    fn assess_not_token_bound_when_nothing_deferred() {
        // No deferral ⇒ not token-bound, not pressured, regardless of health.
        let s = snap(7, 3);
        let a = assess_pressure(Some(&s), 7, 3, 10, 10, 0, DEFAULT_ADVISORY_MIN_QUEUED);
        assert!(!a.token_bound);
        assert!(!a.pressured);
        assert_eq!(a.healthy_accounts, 3);
        assert_eq!(a.exhausted_accounts, 4);
    }

    #[test]
    fn assess_token_bound_when_token_axis_is_min_and_deferred() {
        // token_limit 3 < disk 10 and < ceiling 10, with 4 deferred ⇒ token-bound.
        let s = snap(7, 3);
        let a = assess_pressure(Some(&s), 7, 3, 10, 10, 4, DEFAULT_ADVISORY_MIN_QUEUED);
        assert!(a.token_bound);
        assert!(a.pressured);
        assert_eq!(a.queued, 4);
        // ceil(4/3)=2 waves * 30 = 60 min.
        assert_eq!(a.estimated_drain_minutes, Some(60));
    }

    #[test]
    fn assess_not_token_bound_when_disk_is_the_binding_axis() {
        // disk 2 < token_limit 3 ⇒ the bottleneck is disk, not tokens.
        let s = snap(7, 3);
        let a = assess_pressure(Some(&s), 7, 3, 2, 10, 5, DEFAULT_ADVISORY_MIN_QUEUED);
        assert!(!a.token_bound, "disk binds, so no token advisory");
        assert!(!a.pressured);
    }

    #[test]
    fn assess_not_token_bound_when_ceiling_is_the_binding_axis() {
        // configured_max 2 < token_limit 3 ⇒ operator ceiling binds, not tokens.
        let s = snap(7, 3);
        let a = assess_pressure(Some(&s), 7, 3, 10, 2, 5, DEFAULT_ADVISORY_MIN_QUEUED);
        assert!(!a.token_bound);
        assert!(!a.pressured);
    }

    #[test]
    fn assess_drain_none_when_no_healthy_accounts() {
        // All exhausted (token_limit 0), work queued ⇒ token-bound, no drain ETA.
        let s = snap(4, 0);
        let a = assess_pressure(Some(&s), 4, 0, 10, 10, 3, DEFAULT_ADVISORY_MIN_QUEUED);
        assert!(a.token_bound);
        assert!(a.pressured);
        assert_eq!(a.healthy_accounts, 0);
        assert_eq!(a.estimated_drain_minutes, None);
    }

    #[test]
    fn assess_no_ranking_treats_pool_as_healthy() {
        // No ranking ⇒ pool treated as fully healthy; still token-bound if the
        // pool-sized limit is the min and work is deferred.
        let a = assess_pressure(None, 2, 2, 10, 10, 3, DEFAULT_ADVISORY_MIN_QUEUED);
        assert!(a.token_bound);
        assert_eq!(a.healthy_accounts, 2);
        assert_eq!(a.exhausted_accounts, 0);
        assert_eq!(a.total_accounts, 2);
    }

    #[test]
    fn assess_threshold_gates_pressured() {
        // token_bound but below the queued threshold ⇒ not yet pressured.
        let s = snap(7, 3);
        let a = assess_pressure(Some(&s), 7, 3, 10, 10, 2, 5);
        assert!(a.token_bound);
        assert!(!a.pressured, "2 queued < threshold 5");
    }

    // ------------------------------------------------------------------
    // CapacityAdvisory + formatting
    // ------------------------------------------------------------------

    #[test]
    fn advisory_pressure_names_the_levers() {
        let s = snap(7, 1);
        let a = assess_pressure(Some(&s), 7, 1, 10, 10, 12, DEFAULT_ADVISORY_MIN_QUEUED);
        let adv = CapacityAdvisory::pressure(&a);
        assert!(adv.pressured);
        assert_eq!(adv.queued, 12);
        assert!(adv.message.contains("loom-tokens bootstrap"));
        assert!(adv.message.contains("loom-tokens check --ranking"));
        assert!(adv.message.contains("API credits"));
        assert!(adv.message.contains("12 issue"));
    }

    #[test]
    fn advisory_recovery_is_symmetric() {
        let s = snap(7, 7);
        let a = assess_pressure(Some(&s), 7, 7, 10, 10, 0, DEFAULT_ADVISORY_MIN_QUEUED);
        let adv = CapacityAdvisory::recovery(&a);
        assert!(!adv.pressured);
        assert!(adv.message.contains("restored"));
        assert!(adv.message.contains("7/7"));
    }

    #[test]
    fn advisory_pressure_message_handles_zero_healthy() {
        let s = snap(3, 0);
        let a = assess_pressure(Some(&s), 3, 0, 10, 10, 5, DEFAULT_ADVISORY_MIN_QUEUED);
        let adv = CapacityAdvisory::pressure(&a);
        assert!(adv.message.contains("no healthy accounts"));
    }

    #[test]
    fn format_minutes_variants() {
        assert_eq!(format_minutes(30), "30m");
        assert_eq!(format_minutes(59), "59m");
        assert_eq!(format_minutes(60), "1h");
        assert_eq!(format_minutes(90), "1h 30m");
        assert_eq!(format_minutes(120), "2h");
        assert_eq!(format_minutes(0), "0m");
    }
}
