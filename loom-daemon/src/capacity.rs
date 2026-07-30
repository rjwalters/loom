//! Token-capacity backpressure + add-capacity advisory for the autonomous work
//! finder (Issue #3902, epic #3809).
//!
//! The work finder (#3810) drives approved `loom:issue` work to dispatch, and
//! #3811 bounds its concurrency by `min(token-pool size, disk headroom,
//! configured max)` (a CPU/load term sat in that `min` from #3978 until #4512
//! removed it). That policy treats the token pool as a flat count of
//! `*.token` files — but at scale accounts hit their 5h/7d rate limits and go
//! **exhausted**. Dispatching to an exhausted account produces the startup
//! hangs / mid-build deaths seen while dogfooding. This module makes the
//! daemon treat a genuine token limit as a **capacity signal**:
//!
//! 1. **Slow down** — [`token_axis_limit`] backs the token axis off from the
//!    raw pool size toward the count of *healthy* (`available`) accounts, read
//!    from the rotation ranking file (`.ranking`, resolved via
//!    [`read_ranking`] to whichever pool directory — per-repo or shared
//!    machine-level (#3938) — actually holds the tokens, kept in lock-step
//!    with the writer since #4344). When every account is exhausted the limit
//!    drops to 0 and the finder defers (never hammers an exhausted account). A
//!    single healthy account is the throughput floor, never a halt (operator
//!    refinement on #3902).
//! 2. **Alert** — [`assess_pressure`] derives whether the token axis is the
//!    binding constraint *and* work is queued behind it, and
//!    [`CapacityAdvisory::message`] builds an operator advisory naming the
//!    concrete levers (add accounts + `loom-daemon tokens bootstrap`, or buy API
//!    credits, then `loom-daemon tokens check --ranking`; when accounts are `blocked`
//!    on revoked tokens — where `bootstrap --force` cannot recover — the
//!    advisory instead points at `loom-daemon tokens import-from-monitor --force`).
//!    The advisory surfaces on the daemon status view, the event bus
//!    (`daemon.capacity.advisory`), and the log — deduplicated to fire only on
//!    **state change**.
//! 3. **Recover** — the finder re-reads the ranking every tick (bounded cadence
//!    = the tick interval), so as accounts reset to `available` the limit ramps
//!    back up and the backlog drains automatically, with a symmetric recovery
//!    event/log line.
//!
//! # Why the ranking file (not a network probe)
//!
//! The daemon never performs the slow per-account rate-limit probe itself — that
//! is `loom-daemon tokens check`'s job, run out-of-band (cron / `probe-tokens.sh` /
//! the #4080 self-refresh), which writes the discrete status into
//! `<resolved-pool-dir>/.ranking` (the format-of-record the spawn-time
//! selector already consumes). [`read_ranking`] resolves the **same**
//! directory the writer targets (#3938 per-repo-then-shared precedence,
//! #4344) so the reader and writer can never diverge onto different files.
//! Reading that file keeps this module a fast, non-blocking, filesystem-only
//! read that matches the footing of [`crate::tokens::token_pool_size`] and
//! [`crate::disk_headroom::disk_headroom_limit`].
//!
//! # Near-ceiling granularity
//!
//! The `.ranking` format carries only the discrete status word
//! (`available` / `exhausted` / `rate_limited` / `blocked`), where `exhausted`
//! is already assigned by the probe at 7d utilization ≥ 0.95. A finer
//! "near-ceiling ≥ 0.90 but not yet exhausted" bucket would require the richer
//! per-account utilization JSON (`loom-daemon tokens check --json`); this module treats
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

/// Parse the rotation ranking file directly at `{pool_dir}/.ranking`.
///
/// This is the resolved-dir-aware core of [`read_ranking`] (issue #4344):
/// `pool_dir` is already resolved (e.g. via
/// [`crate::tokens_pool::paths::resolve_tokens_dir`]) to whichever pool —
/// per-repo or shared machine-level (#3938) — actually holds the `*.token`
/// files, so callers that have their own resolved directory (or want to probe
/// a specific one directly, e.g. in tests) can bypass re-resolution.
///
/// The file is the pipe-delimited `name|status|5h_util` format written by
/// `loom-daemon tokens check --ranking` (one account per line, issue #4195), where the
/// status is the **second** field and the trailing 5h-utilization field is
/// optional (legacy `name|status` lines omit it). Returns `None` when the file
/// is absent, unreadable, or contains no parseable rows — the signal that no
/// probe data exists, in which case callers fall back to the raw token-pool size
/// (byte-for-byte the pre-#3902 behavior). Blank lines, `#` comments, and
/// malformed rows (no `|`, so no status field) are skipped; a row's status is
/// parsed via [`AccountHealth::parse`].
///
/// Parsing goes through [`crate::tokens_pool::select::parse_ranking_line`] — the
/// **same** triple parser the spawn-time selector uses — so this reader and the
/// selector can never de-sync on the field layout (the #4243/#4344 drift, where
/// this function took the *last* field as the status and mis-read every 3-field
/// row's `5h_util` — e.g. `agent3-2amlogic|available|0.09` → `"0.09"` →
/// [`AccountHealth::Unknown`] — pinning the token axis at 0 on a fully healthy
/// pool). A malformed no-`|` row is still skipped here (rather than counted with
/// an empty/unknown status) so an unparseable ranking is treated as absent, not
/// as a genuine 0-healthy snapshot.
#[must_use]
pub fn read_ranking_at(pool_dir: &Path) -> Option<RankingSnapshot> {
    let ranking_path = pool_dir.join(".ranking");
    let contents = std::fs::read_to_string(&ranking_path).ok()?;
    let mut snap = RankingSnapshot::default();
    for line in contents.lines() {
        // A row must carry a `|` (a status field). A no-`|` row is genuinely
        // malformed and is skipped, not counted as an empty-status account —
        // preserving the "unparseable ranking ⇒ absent, not 0-healthy"
        // fallback (see the module doc + `read_ranking_malformed_*` tests).
        if !line.contains('|') {
            continue;
        }
        let Some((_name, status, _util_5h)) = crate::tokens_pool::select::parse_ranking_line(line)
        else {
            continue;
        };
        snap.total += 1;
        match AccountHealth::parse(&status) {
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

/// Parse the rotation ranking file for `workspace_root`, resolving the
/// effective pool directory the **same way** [`crate::tokens::token_pool_size`]
/// and [`crate::tokens_pool::paths::resolve_tokens_dir`] do: the per-repo pool
/// (`{workspace_root}/.loom/tokens/`) when it holds `*.token` files, else the
/// shared machine-level pool (`~/.loom/tokens`, override
/// `LOOM_SHARED_TOKENS_DIR`, issue #3938), else the per-repo path unchanged.
///
/// # Why this matters (issue #4344)
///
/// Before this fix, this function hardcoded the per-repo path regardless of
/// where the pool (and the ranking the writer refreshes) actually lived. On a
/// host where the pool resolves to the shared machine-level directory (the
/// per-repo dir has no `*.token` files), every ranking refresh — manual or the
/// #4080 self-refresh — landed in the shared `.ranking`, while this reader kept
/// consulting a stale, orphaned per-repo `.ranking` left over from a prior
/// storm. If that orphaned file had 0 `available` rows, the token axis was
/// pinned at 0 **forever** — re-read every tick, always from the wrong file,
/// surviving even a daemon restart (the orphaned file itself never changes).
/// Resolving the same directory the writer targets closes that gap; the
/// fallback semantics (absent/unparseable ranking anywhere ⇒ `None`, callers
/// fall back to the raw pool size) are unchanged.
#[must_use]
pub fn read_ranking(workspace_root: &Path) -> Option<RankingSnapshot> {
    read_ranking_at(&crate::tokens_pool::paths::resolve_tokens_dir(workspace_root))
}

// ============================================================================
// Fresh-probe capacity (issue #3936)
// ============================================================================

/// Near-ceiling 7d-utilization threshold: an account at or above this fraction
/// is treated as exhausted/near-ceiling and **never** counted healthy, mirroring
/// `loom-tokens`' `EXHAUSTED_THRESHOLD` (`check.py`). Kept here so the daemon's
/// status view applies the *same* threshold the probe uses when it assigns the
/// discrete status word — closing the #3936 gap where a `99%`-7d account whose
/// status word said `available` was still counted healthy.
pub const NEAR_CEILING_THRESHOLD: f64 = 0.95;

/// Decide whether a single probed account is healthy under the unified rule.
///
/// An account is healthy **only** when its probe status word is `available`
/// *and* its 7d utilization is below [`NEAR_CEILING_THRESHOLD`]. The utilization
/// override is what fixes #3936: a stale or monitor-sourced `available` status
/// on an account that has actually reached the 7d ceiling (e.g. `99%`) is
/// treated as unhealthy anyway, so the summary count and the per-token table can
/// never disagree about it.
#[must_use]
pub fn probe_account_healthy(status: &str, util_7d: Option<f64>) -> bool {
    if let Some(u) = util_7d {
        if u >= NEAR_CEILING_THRESHOLD {
            return false;
        }
    }
    status.trim() == "available"
}

/// The status word to *display* for an account, applying the same near-ceiling
/// override as [`probe_account_healthy`]. An `available` account at/above the
/// 7d ceiling renders as `exhausted` so the per-token table never shows a row
/// that contradicts the healthy count (#3936). Every other status word is
/// passed through unchanged.
#[must_use]
pub fn effective_probe_status(status: &str, util_7d: Option<f64>) -> String {
    let trimmed = status.trim();
    if trimmed == "available" {
        if let Some(u) = util_7d {
            if u >= NEAR_CEILING_THRESHOLD {
                return "exhausted".to_string();
            }
        }
    }
    trimmed.to_string()
}

/// Aggregate healthy/exhausted counts derived from a fresh `loom-daemon tokens check
/// --json` probe, applying [`probe_account_healthy`] uniformly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeCapacity {
    /// Total accounts in the probe.
    pub total: usize,
    /// Accounts healthy under the unified rule (available AND below ceiling).
    pub healthy: usize,
    /// Accounts not healthy — `total - healthy`.
    pub exhausted: usize,
}

/// Summarize a fresh probe into a [`ProbeCapacity`].
///
/// Each item is the account's `(status_word, 7d_utilization)` as read from the
/// `loom-daemon tokens check --json` accounts array. This is the single source of truth
/// the status view uses for the capacity summary, the healthy-tokens cap input,
/// and (via [`effective_probe_status`]) the per-token table — so all three are
/// computed from the same source and the same threshold and cannot contradict
/// each other (#3936, acceptance criterion 1).
#[must_use]
pub fn summarize_probe<'a, I>(accounts: I) -> ProbeCapacity
where
    I: IntoIterator<Item = (&'a str, Option<f64>)>,
{
    let mut total = 0usize;
    let mut healthy = 0usize;
    for (status, util_7d) in accounts {
        total += 1;
        if probe_account_healthy(status, util_7d) {
            healthy += 1;
        }
    }
    ProbeCapacity {
        total,
        healthy,
        exhausted: total - healthy,
    }
}

/// The health-adjusted token-axis concurrency limit.
///
/// When ranking data exists, this is the count of `available` accounts — the
/// finder never dispatches beyond the healthy set, so it never targets an
/// exhausted/blocked account and never over-subscribes. When ranking data is
/// absent (no probe has run), it falls back to `pool_size` — the pre-#3902
/// behavior, so a repo that has not wired up `loom-daemon tokens check` sees zero
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
/// - `disk` / `configured_max` — the other two dynamic-cap axes. (A `cpu` axis
///   sat here from #3978 until #4512 removed the CPU term from admission — see
///   [`crate::work_finder::resolve_dynamic_max_concurrent`].)
/// - `deferred` — issues the tick deferred for capacity ([`crate`]'s
///   `TickReport::deferred_capacity`).
/// - `min_queued` — the advisory threshold ([`DEFAULT_ADVISORY_MIN_QUEUED`]).
///
/// `token_bound` requires that the token axis is the (co-)minimum of every cap
/// axis: dispatch is held back by tokens specifically, not by a full disk or the
/// operator ceiling. This keeps the advisory from firing (and misleadingly
/// telling the operator to add token accounts) when the real bottleneck is disk
/// or a deliberately low `maxConcurrent`.
#[must_use]
#[allow(clippy::too_many_arguments)] // one axis per dynamic-cap input + the
                                     // advisory threshold; grouping them into a
                                     // struct would obscure the direct mapping
                                     // to `resolve_dynamic_max_concurrent`'s args.
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
             Add accounts to ~/.claude-monitor/accounts.env then `loom-daemon tokens bootstrap`, or buy \
             API credits, then re-probe with `loom-daemon tokens check --ranking`. If accounts are \
             'blocked' on revoked tokens, 'bootstrap --force' cannot recover — run \
             'loom-daemon tokens import-from-monitor --force && loom-daemon tokens check --ranking'.",
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
    use crate::tokens_pool::paths::SHARED_TOKENS_DIR_ENV;
    use serial_test::serial;
    use std::fs;
    use std::path::Path;

    /// Write a `.ranking` file directly into `pool_dir` (no `.loom/tokens`
    /// nesting) — the fixture for [`read_ranking_at`], which is
    /// resolution-agnostic.
    fn write_ranking_at(pool_dir: &Path, body: &str) {
        fs::create_dir_all(pool_dir).unwrap();
        fs::write(pool_dir.join(".ranking"), body).unwrap();
    }

    /// Write a `.ranking` file at the legacy per-repo location
    /// (`{workspace}/.loom/tokens/.ranking`) — the fixture for [`read_ranking`]
    /// resolution tests.
    fn write_ranking(workspace: &Path, body: &str) {
        write_ranking_at(&workspace.join(".loom").join("tokens"), body);
    }

    /// Write a `.token` file into `dir` so [`crate::tokens_pool::paths::
    /// resolve_tokens_dir`] treats it as a populated pool.
    fn write_token_file(dir: &Path, name: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), "sk-ant-oat01-fake").unwrap();
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
    // read_ranking_at (pure parsing — no directory resolution, no env)
    // ------------------------------------------------------------------

    #[test]
    fn read_ranking_at_missing_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_ranking_at(tmp.path()), None);
    }

    #[test]
    fn read_ranking_at_empty_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_ranking_at(tmp.path(), "\n  \n");
        assert_eq!(read_ranking_at(tmp.path()), None);
    }

    #[test]
    fn read_ranking_at_counts_statuses() {
        let tmp = tempfile::tempdir().unwrap();
        write_ranking_at(
            tmp.path(),
            "a|available\nb|available\nc|exhausted\nd|rate_limited\ne|blocked\nf|weird\n",
        );
        let snap = read_ranking_at(tmp.path()).unwrap();
        assert_eq!(snap.total, 6);
        assert_eq!(snap.available, 2);
        assert_eq!(snap.exhausted, 1);
        assert_eq!(snap.rate_limited, 1);
        assert_eq!(snap.blocked, 1);
        assert_eq!(snap.unknown, 1);
        assert_eq!(snap.unhealthy(), 4);
    }

    #[test]
    fn read_ranking_at_skips_malformed_rows() {
        let tmp = tempfile::tempdir().unwrap();
        write_ranking_at(tmp.path(), "a|available\nno-pipe-here\n\nb|exhausted\n");
        let snap = read_ranking_at(tmp.path()).unwrap();
        assert_eq!(snap.total, 2, "the pipe-less row is skipped");
        assert_eq!(snap.available, 1);
        assert_eq!(snap.exhausted, 1);
    }

    #[test]
    fn read_ranking_at_counts_never_used_account_as_healthy() {
        // Regression test for issue #4645: the monitor writer used to emit a
        // malformed `name|` row (empty status) for a manifest account with no
        // usage history (e.g. freshly bootstrapped/never-used). The fixed
        // writer (`tokens_pool::monitor::build_monitor_accounts`) now emits
        // `name|available` for such accounts, so the daemon's healthy count
        // must include them rather than silently dropping the row as
        // malformed (a no-`|` row) or counting it unhealthy.
        let tmp = tempfile::tempdir().unwrap();
        write_ranking_at(tmp.path(), "agent4-2amlogic|exhausted|0.18\nagent6-2amlogic|available\n");
        let snap = read_ranking_at(tmp.path()).unwrap();
        assert_eq!(snap.total, 2);
        assert_eq!(
            snap.available, 1,
            "the never-used account's `available` row must count healthy"
        );
        assert_eq!(snap.exhausted, 1);
    }

    #[test]
    fn read_ranking_at_parses_three_field_format() {
        // #4243/#4344 primary bug: writers emit `name|status|5h_util` whenever
        // 5h utilization is known (which on the live host is always). The
        // status is the SECOND field — the reader must NOT take the trailing
        // util as the status. This is the exact live-host layout from the
        // #4344 incident: 7 three-field rows, 6 available.
        let tmp = tempfile::tempdir().unwrap();
        write_ranking_at(
            tmp.path(),
            "agent3-2amlogic|available|0.09\n\
             robb-2amlogic|available|0.18\n\
             rjwalters-gmail|available|0.22\n\
             agent2-2amlogic|available|0.31\n\
             agent4-2amlogic|available|0.05\n\
             agent5-2amlogic|available|0.44\n\
             old-account|exhausted|0.00\n",
        );
        let snap = read_ranking_at(tmp.path()).unwrap();
        assert_eq!(snap.total, 7);
        assert_eq!(snap.available, 6, "3-field rows: the status is the SECOND field, not the last");
        assert_eq!(snap.exhausted, 1);
        assert_eq!(
            snap.unknown, 0,
            "the 5h_util field must never be parsed as a status word (the #4243 drift)"
        );
    }

    #[test]
    fn read_ranking_at_curator_fixture_three_field() {
        // The exact fixture the judge required to pass:
        //   total=3, available=2, exhausted=1.
        let tmp = tempfile::tempdir().unwrap();
        write_ranking_at(
            tmp.path(),
            "agent3-2amlogic|available|0.09\n\
             robb-2amlogic|available|0.18\n\
             rjwalters-gmail|exhausted|0.00\n",
        );
        let snap = read_ranking_at(tmp.path()).unwrap();
        assert_eq!(snap.total, 3);
        assert_eq!(snap.available, 2);
        assert_eq!(snap.exhausted, 1);
    }

    #[test]
    fn read_ranking_at_mixed_two_and_three_field() {
        // A file mixing legacy 2-field lines and new 3-field lines — the state
        // during a rolling format migration — must parse both correctly.
        let tmp = tempfile::tempdir().unwrap();
        write_ranking_at(
            tmp.path(),
            "legacy-a|available\n\
             new-b|available|0.12\n\
             legacy-c|exhausted\n\
             new-d|blocked|0.00\n\
             new-e|rate_limited|0.88\n",
        );
        let snap = read_ranking_at(tmp.path()).unwrap();
        assert_eq!(snap.total, 5);
        assert_eq!(snap.available, 2);
        assert_eq!(snap.exhausted, 1);
        assert_eq!(snap.blocked, 1);
        assert_eq!(snap.rate_limited, 1);
        assert_eq!(snap.unknown, 0);
    }

    #[test]
    fn read_ranking_at_comment_lines_are_skipped() {
        // `#` comments (stripped by the shared selector parser) are not rows.
        let tmp = tempfile::tempdir().unwrap();
        write_ranking_at(
            tmp.path(),
            "# probed 2026-07-29\na|available|0.10\nb|exhausted # near ceiling\n",
        );
        let snap = read_ranking_at(tmp.path()).unwrap();
        assert_eq!(snap.total, 2, "the leading comment line is not a row");
        assert_eq!(snap.available, 1);
        assert_eq!(snap.exhausted, 1);
    }

    #[test]
    fn ranking_format_drift_conformance_writer_reader_selector_agree() {
        // Format-drift guard (#4344 AC 3): the single test that would have
        // caught #4243's partial migration. It binds THREE parties to the same
        // fixture lines and fails if any one drifts:
        //   (1) the WRITER — `tokens_pool::check::ranking_line` (what is
        //       actually written to `.ranking`),
        //   (2) capacity's READER — `read_ranking_at` (this module),
        //   (3) the spawn-time SELECTOR's parser —
        //       `tokens_pool::select::parse_ranking_line`.
        // If a future format change moves the status field, at least one of
        // these assertions breaks, so no reader can silently miss the
        // migration again.
        use crate::tokens_pool::check::ranking_line;
        use crate::tokens_pool::select::parse_ranking_line;

        // (name, status, util) fixtures spanning every health class plus the
        // optional / absent util field.
        let accounts: [(&str, &str, Option<f64>); 5] = [
            ("agent3-2amlogic", "available", Some(0.09)),
            ("robb-2amlogic", "available", None), // legacy 2-field line
            ("rjwalters-gmail", "exhausted", Some(0.00)),
            ("agent2", "rate_limited", Some(0.88)),
            ("agent4", "blocked", None),
        ];

        // Render the file exactly as the writer would.
        let body: String = accounts
            .iter()
            .map(|(name, status, util)| ranking_line(name, status, *util) + "\n")
            .collect();

        // (2) vs (3): the selector's parser must recover the same name/status
        // the writer put in, from the writer's own output.
        for ((name, status, util), line) in accounts.iter().zip(body.lines()) {
            let (pname, pstatus, putil) =
                parse_ranking_line(line).expect("writer output must be parseable by the selector");
            assert_eq!(&pname, name);
            assert_eq!(
                &pstatus, status,
                "selector parser must recover the writer's status field, not the util"
            );
            match util {
                Some(u) => {
                    let p = putil.expect("a 3-field line must yield a util");
                    assert_eq!(format!("{p:.2}"), format!("{u:.2}"));
                }
                None => assert_eq!(putil, None, "a 2-field line must yield no util"),
            }
        }

        // (1) → (2): capacity's reader must classify that same file into the
        // writer's intended statuses — 2 available, 1 exhausted, 1
        // rate_limited, 1 blocked, and crucially 0 unknown.
        let tmp = tempfile::tempdir().unwrap();
        write_ranking_at(tmp.path(), &body);
        let snap = read_ranking_at(tmp.path()).unwrap();
        assert_eq!(snap.total, 5);
        assert_eq!(snap.available, 2);
        assert_eq!(snap.exhausted, 1);
        assert_eq!(snap.rate_limited, 1);
        assert_eq!(snap.blocked, 1);
        assert_eq!(
            snap.unknown, 0,
            "no writer-produced line may parse to Unknown — that is the #4243 drift signature"
        );
    }

    // ------------------------------------------------------------------
    // read_ranking (resolved-dir-aware) — issue #4344
    //
    // These tests exercise directory *resolution*, which reads
    // `LOOM_SHARED_TOKENS_DIR` (a process-global env var). Serialized against
    // every other `#[serial]` test in the crate (unkeyed group, matching
    // `tokens_pool::paths`' own tests) and always pinned to an explicit value
    // — never left ambient — so a real `~/.loom/tokens` on the host running
    // the test suite can never leak in.
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn read_ranking_per_repo_pool_unchanged() {
        // Per-repo tokens + per-repo ranking: resolution must still pick the
        // per-repo pool, matching pre-#4344 behavior byte-for-byte.
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");
        let repo = tempfile::tempdir().unwrap();
        write_token_file(&repo.path().join(".loom").join("tokens"), "a.token");
        write_ranking(repo.path(), "a|available\nb|exhausted\n");

        let snap = read_ranking(repo.path()).unwrap();
        assert_eq!(snap.total, 2);
        assert_eq!(snap.available, 1);
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn read_ranking_resolves_shared_pool_over_stale_per_repo_ranking() {
        // The exact #4344 regression fixture: a shared-pool layout (per-repo
        // dir has NO `*.token` files, so it never wins resolution) with a
        // stale, orphaned per-repo `.ranking` reporting 0 healthy, and a
        // fresh shared `.ranking` reporting N healthy. Reading via the
        // per-repo path alone (pre-#4344) would return the stale 0-healthy
        // snapshot forever; resolution must instead follow the pool the
        // refresher actually writes to.
        let repo = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        write_token_file(shared.path(), "s.token");
        write_ranking_at(shared.path(), "s1|available\ns2|available\ns3|available\n");
        // Orphaned per-repo ranking: 0 healthy, would starve dispatch forever
        // if it were the one actually consulted.
        write_ranking(repo.path(), "orphan|exhausted\n");
        std::env::set_var(SHARED_TOKENS_DIR_ENV, shared.path().to_str().unwrap());

        let snap = read_ranking(repo.path()).unwrap();
        assert_eq!(
            snap.available, 3,
            "must read the shared pool's fresh ranking, not the stale per-repo one"
        );
        assert_eq!(snap.total, 3);
        assert_eq!(token_axis_limit(repo.path(), 3), 3);
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn dispatch_resumes_after_ranking_refresh_without_restart() {
        // Filed AC 4 (regression test), sharpened by the curator analysis:
        // simulates the exact incident timeline — the daemon "starts" (first
        // tick) with a 0-healthy ranking present, then the ranking file is
        // refreshed to N-healthy minutes later (no daemon restart in
        // between). Each `token_axis_limit` call below stands in for one
        // work-finder tick — there is no in-memory cache to invalidate, so
        // the very next tick after the refresh must observe the new count.
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");
        let repo = tempfile::tempdir().unwrap();
        let per_repo = repo.path().join(".loom").join("tokens");
        write_token_file(&per_repo, "a.token");
        write_token_file(&per_repo, "b.token");
        write_token_file(&per_repo, "c.token");

        // "Startup" tick: a storm-era ranking present with 0 healthy accounts.
        write_ranking_at(&per_repo, "a|exhausted\nb|exhausted\nc|blocked\n");
        assert_eq!(
            token_axis_limit(repo.path(), 3),
            0,
            "tick 1 (at 'startup'): dispatch correctly sees 0 healthy — matches the incident's \
             initial wedge"
        );

        // Minutes later, the ranking is refreshed to 2 healthy — no restart,
        // just an on-disk file change (mirrors a manual `loom-daemon tokens check
        // --ranking` or the #4080 self-refresh).
        write_ranking_at(&per_repo, "a|available\nb|available\nc|exhausted\n");
        assert_eq!(
            token_axis_limit(repo.path(), 3),
            2,
            "tick 2 (next tick after the refresh): dispatch resumes at the new healthy count \
             within one tick, with no restart"
        );
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn read_ranking_absent_everywhere_falls_back_to_none() {
        // Neither pool holds `*.token` files nor a `.ranking` — resolution
        // lands on the per-repo path (resolve_tokens_dir's own fallback), and
        // no ranking exists there either. Preserves the absent-ranking
        // fallback policy: callers see `None` and fall back to raw pool size.
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");
        let repo = tempfile::tempdir().unwrap();
        assert_eq!(read_ranking(repo.path()), None);
        assert_eq!(
            token_axis_limit(repo.path(), 5),
            5,
            "no ranking ⇒ raw pool size (pre-#3902 fallback)"
        );
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn read_ranking_both_pools_have_ranking_per_repo_wins() {
        // Both the per-repo and shared pools hold token files (and a
        // ranking); resolution must prefer the per-repo pool, matching
        // `resolve_tokens_dir`'s precedence.
        let repo = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        let per_repo = repo.path().join(".loom").join("tokens");
        write_token_file(&per_repo, "a.token");
        write_ranking_at(&per_repo, "a|available\n");
        write_token_file(shared.path(), "s.token");
        write_ranking_at(shared.path(), "s1|available\ns2|available\ns3|available\n");
        std::env::set_var(SHARED_TOKENS_DIR_ENV, shared.path().to_str().unwrap());

        let snap = read_ranking(repo.path()).unwrap();
        assert_eq!(
            snap.total, 1,
            "per-repo pool wins when it has tokens, even if shared also has some"
        );
        assert_eq!(snap.available, 1);
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn read_ranking_malformed_ranking_is_absent_not_zero() {
        // A ranking file with no parseable rows is treated as absent (`None`)
        // rather than a genuine 0-healthy snapshot, so token_axis_limit falls
        // back to the raw pool size instead of wrongly pinning to 0.
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");
        let repo = tempfile::tempdir().unwrap();
        write_ranking(repo.path(), "\n  \nno-pipe-here\n");
        assert_eq!(read_ranking(repo.path()), None);
        assert_eq!(token_axis_limit(repo.path(), 4), 4);
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    // ------------------------------------------------------------------
    // token_axis_limit
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn token_axis_limit_uses_available_when_ranking_present() {
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        let per_repo = tmp.path().join(".loom").join("tokens");
        write_token_file(&per_repo, "a.token");
        write_token_file(&per_repo, "b.token");
        write_token_file(&per_repo, "c.token");
        write_ranking_at(&per_repo, "a|available\nb|available\nc|exhausted\n");
        // 3 token files present, but only 2 available → limit 2.
        assert_eq!(token_axis_limit(tmp.path(), 3), 2);
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn token_axis_limit_falls_back_to_pool_when_no_ranking() {
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        // No ranking file → fall back to raw pool size (pre-#3902 behavior).
        assert_eq!(token_axis_limit(tmp.path(), 5), 5);
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn token_axis_limit_zero_when_all_exhausted() {
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        let per_repo = tmp.path().join(".loom").join("tokens");
        write_token_file(&per_repo, "a.token");
        write_token_file(&per_repo, "b.token");
        write_ranking_at(&per_repo, "a|exhausted\nb|blocked\n");
        assert_eq!(token_axis_limit(tmp.path(), 2), 0, "never dispatch to exhausted");
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
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
        // token_limit 3 < disk 10 and ceiling 10, with 4 deferred ⇒ token-bound.
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
        assert!(adv.message.contains("loom-daemon tokens bootstrap"));
        assert!(adv.message.contains("loom-daemon tokens check --ranking"));
        assert!(adv.message.contains("API credits"));
        assert!(adv.message.contains("12 issue"));
        assert!(adv.message.contains("import-from-monitor"));
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

    // ------------------------------------------------------------------
    // Fresh-probe capacity (issue #3936)
    // ------------------------------------------------------------------

    #[test]
    fn probe_account_healthy_applies_near_ceiling_override() {
        // A genuinely-available account below the ceiling is healthy.
        assert!(probe_account_healthy("available", Some(0.10)));
        assert!(probe_account_healthy("available", Some(0.94)));
        assert!(probe_account_healthy("available", None));
        // At/above the 95% near-ceiling threshold it is NEVER healthy, even
        // though the probe's status word still says `available` (#3936).
        assert!(!probe_account_healthy("available", Some(0.95)));
        assert!(!probe_account_healthy("available", Some(0.99)));
        assert!(!probe_account_healthy("available", Some(1.00)));
        // Non-`available` status words are never healthy regardless of util.
        assert!(!probe_account_healthy("exhausted", Some(0.10)));
        assert!(!probe_account_healthy("rate_limited", Some(0.10)));
        assert!(!probe_account_healthy("blocked", None));
        assert!(!probe_account_healthy("error", None));
        // Whitespace tolerance.
        assert!(probe_account_healthy(" available ", Some(0.10)));
    }

    #[test]
    fn effective_probe_status_corrects_near_ceiling_available() {
        // The 99%-7d `available` row renders as `exhausted` so the table never
        // contradicts the healthy count (#3936: agent3-2amlogic at 99%).
        assert_eq!(effective_probe_status("available", Some(0.99)), "exhausted");
        assert_eq!(effective_probe_status("available", Some(0.95)), "exhausted");
        // Below the ceiling it stays `available`.
        assert_eq!(effective_probe_status("available", Some(0.50)), "available");
        assert_eq!(effective_probe_status("available", None), "available");
        // Other status words pass through untouched.
        assert_eq!(effective_probe_status("exhausted", Some(1.0)), "exhausted");
        assert_eq!(effective_probe_status("rate_limited", None), "rate_limited");
        assert_eq!(effective_probe_status("blocked", None), "blocked");
    }

    #[test]
    fn summarize_probe_mixed_pool_is_self_consistent() {
        // A mixed pool: some available-and-low, one near-ceiling `available`
        // (the #3936 mislabel), some exhausted, one rate_limited. This mirrors
        // the live incident (2 truly available, 5 not) and asserts the summary
        // agrees with a table rendered from the SAME source + threshold.
        let accounts: Vec<(&str, Option<f64>)> = vec![
            ("available", Some(0.10)),    // healthy
            ("available", Some(0.42)),    // healthy
            ("available", Some(0.99)),    // near-ceiling mislabel -> NOT healthy
            ("exhausted", Some(1.00)),    // not healthy
            ("exhausted", Some(1.00)),    // not healthy
            ("exhausted", Some(0.97)),    // not healthy
            ("rate_limited", Some(0.30)), // not healthy
        ];

        let cap = summarize_probe(accounts.iter().copied());
        assert_eq!(cap.total, 7);
        assert_eq!(cap.healthy, 2, "only the two below-ceiling `available` rows");
        assert_eq!(cap.exhausted, 5);
        assert_eq!(cap.healthy + cap.exhausted, cap.total);

        // Table/summary consistency: the number of rows the table renders as an
        // effective `available` status must equal the summary's healthy count,
        // and NO row shows `available` while being counted unhealthy (#3936).
        let table_available = accounts
            .iter()
            .filter(|(s, u)| effective_probe_status(s, *u) == "available")
            .count();
        assert_eq!(
            table_available, cap.healthy,
            "per-token table `available` rows must match the healthy summary count"
        );
        for (s, u) in &accounts {
            let shown = effective_probe_status(s, *u);
            if shown == "available" {
                assert!(
                    probe_account_healthy(s, *u),
                    "a row shown `available` must be counted healthy"
                );
            }
        }
    }

    #[test]
    fn summarize_probe_empty_is_zero() {
        let cap = summarize_probe(std::iter::empty());
        assert_eq!(cap, ProbeCapacity::default());
        assert_eq!(cap.total, 0);
        assert_eq!(cap.healthy, 0);
        assert_eq!(cap.exhausted, 0);
    }

    #[test]
    fn summarize_probe_all_healthy_and_all_exhausted() {
        let all_ok: Vec<(&str, Option<f64>)> =
            vec![("available", Some(0.1)), ("available", Some(0.2))];
        let cap = summarize_probe(all_ok.iter().copied());
        assert_eq!((cap.total, cap.healthy, cap.exhausted), (2, 2, 0));

        let all_bad: Vec<(&str, Option<f64>)> =
            vec![("exhausted", Some(1.0)), ("available", Some(0.96))];
        let cap = summarize_probe(all_bad.iter().copied());
        assert_eq!((cap.total, cap.healthy, cap.exhausted), (2, 0, 2));
    }
}
