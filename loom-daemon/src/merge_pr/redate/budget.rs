//! The bounded re-date budget and its backoff (#9590).
//!
//! #8508 allowed ONE re-date per head: the re-date moves the head onto the SHA
//! its own marker names, so the very next freshness block on that head
//! escalated to `loom:operator`. On a base branch that moves faster than CI
//! runs (2026-09-29/30: `main` every 15–30 min, CI ~15 min), that turned
//! about ten Judge-approved PRs into operator holds with no decision in them.
//!
//! This module replaces "one, then escalate" with a budget of N re-dates per
//! **chain**, with exponential backoff between them:
//!
//! - A *chain* is the run of consecutive tree-identical re-date commits that
//!   started from a head Loom did not create. Each re-date records its
//!   position in the chain as durable forge state, in an attempt marker
//!   ([`attempt_marker`]) beside #8508's legacy marker, so no process owns
//!   the count and a daemon restart cannot reset it.
//! - Any other push (human, Builder, Doctor, head-sync) lands on a head with
//!   no marker, which starts a fresh chain. The documented human release
//!   ("push any commit") therefore still works, and only a push from outside
//!   the remedy can reset the budget, so the remedy can never loop on its own.
//! - Re-date `k+1` is pushed only once `backoff * 2^(k-1)` has elapsed since
//!   the comment that recorded re-date `k`. Before that the remedy defers:
//!   it writes nothing, escalates nothing, and the merge is retried later.
//! - Once the chain has spent the whole budget, the PR escalates exactly as
//!   before (#8508).
//!
//! Only trusted authors' markers are read (#9548/#9593); an outsider's marker
//! can neither spend the budget nor trigger an escalation.

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::Value;
use std::path::Path;

/// Default re-date budget per chain.
pub const DEFAULT_BUDGET: u32 = 3;
/// Upper clamp on a configured budget. The bound is the point of the
/// mechanism, so a typo like `300` must not quietly make it meaningless.
pub const MAX_BUDGET: u32 = 10;
/// Default backoff base, in seconds, before the second re-date of a chain.
pub const DEFAULT_BACKOFF_SECS: u64 = 600;
/// Upper clamp on a configured backoff base (one day).
pub const MAX_BACKOFF_SECS: u64 = 86_400;

/// Env override for the budget (beats config).
pub const BUDGET_ENV: &str = "LOOM_REDATE_BUDGET";
/// Env override for the backoff base (beats config).
pub const BACKOFF_ENV: &str = "LOOM_REDATE_BACKOFF_SECS";
/// Config key for the budget.
pub const BUDGET_CONFIG_KEY: &str = "champion.redateBudget";
/// Config key for the backoff base.
pub const BACKOFF_CONFIG_KEY: &str = "champion.redateBackoffSecs";

/// The resolved budget parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetConfig {
    /// Re-date pushes allowed per chain, `1..=MAX_BUDGET`.
    pub budget: u32,
    /// Backoff base in seconds, `0..=MAX_BACKOFF_SECS`.
    pub backoff_secs: u64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            budget: DEFAULT_BUDGET,
            backoff_secs: DEFAULT_BACKOFF_SECS,
        }
    }
}

fn valid_budget(n: u64) -> Option<u32> {
    (n >= 1).then(|| u32::try_from(n.min(u64::from(MAX_BUDGET))).unwrap_or(MAX_BUDGET))
}

impl BudgetConfig {
    /// env > config > default. An invalid value at any tier (unparseable, a
    /// budget below 1, a negative backoff) falls through to the next tier.
    #[must_use]
    pub fn resolve(env_budget: Option<&str>, env_backoff: Option<&str>, config: &Value) -> Self {
        let get = |key| crate::config_resolver::get_path(config, key).and_then(Value::as_u64);
        let budget = env_budget
            .and_then(|s| s.trim().parse::<u64>().ok())
            .and_then(valid_budget)
            .or_else(|| get(BUDGET_CONFIG_KEY).and_then(valid_budget))
            .unwrap_or(DEFAULT_BUDGET);
        let backoff_secs = env_backoff
            .and_then(|s| s.trim().parse::<u64>().ok())
            .or_else(|| get(BACKOFF_CONFIG_KEY))
            .unwrap_or(DEFAULT_BACKOFF_SECS)
            .min(MAX_BACKOFF_SECS);
        Self {
            budget,
            backoff_secs,
        }
    }

    /// The parameters for the workspace at `root`, from the process env and
    /// its effective config.
    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        let effective = crate::config_resolver::resolve_effective_config(root);
        Self::resolve(
            std::env::var(BUDGET_ENV).ok().as_deref(),
            std::env::var(BACKOFF_ENV).ok().as_deref(),
            &effective,
        )
    }

    /// How long to wait after re-date `n` (1-based) before re-date `n+1`.
    #[must_use]
    pub fn backoff_after(&self, n: u32) -> TimeDelta {
        let factor = 1u64 << n.saturating_sub(1).min(20);
        let secs = self.backoff_secs.saturating_mul(factor);
        TimeDelta::try_seconds(i64::try_from(secs).unwrap_or(i64::MAX)).unwrap_or(TimeDelta::MAX)
    }
}

/// The chain-position marker recorded beside #8508's legacy marker.
///
/// A separate marker rather than an extra field on the legacy one, so an
/// older daemon in a mixed fleet still finds `to=<sha> -->` and escalates
/// conservatively instead of re-pushing on a head a newer daemon re-dated.
#[must_use]
pub fn attempt_marker(new_sha: &str, n: u32) -> String {
    format!("<!-- loom:stale-check-redate-attempt to={new_sha} n={n} -->")
}

/// Where the current head sits in its re-date chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainPosition {
    /// Re-dates already spent in this chain: 0 when the head carries no
    /// trusted marker (a push from outside the remedy started a fresh chain).
    pub spent: u32,
    /// When the comment recording re-date `spent` was posted, if known.
    pub recorded_at: Option<DateTime<Utc>>,
}

/// The chain position of `head`, from a TRUSTED comment listing (REST
/// objects with `body` and `created_at`). A legacy-only marker (#8508, no
/// attempt marker) counts as position 1.
#[must_use]
pub fn chain_position(comments: &[Value], head: &str) -> ChainPosition {
    let attempt_prefix = format!("<!-- loom:stale-check-redate-attempt to={head} n=");
    let legacy = super::redate_marker(head);
    let mut best = ChainPosition {
        spent: 0,
        recorded_at: None,
    };
    for c in comments {
        let Some(body) = c.get("body").and_then(Value::as_str) else {
            continue;
        };
        let n = body
            .split(&attempt_prefix)
            .skip(1)
            .filter_map(|rest| rest.split_once(" -->"))
            .filter_map(|(digits, _)| digits.parse::<u32>().ok())
            .max()
            .or_else(|| body.contains(&legacy).then_some(1));
        let Some(n) = n else { continue };
        let at = c
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));
        if n > best.spent || (n == best.spent && at > best.recorded_at) {
            best = ChainPosition {
                spent: n,
                recorded_at: at,
            };
        }
    }
    best
}

/// What the remedy may do for a head at `pos`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetDecision {
    /// Push re-date number `n` of the chain.
    Push { n: u32 },
    /// Inside the backoff window after the last re-date: write nothing.
    Defer { retry_after: DateTime<Utc> },
    /// The chain has spent the whole budget: escalate.
    Exhausted { spent: u32 },
}

/// Pure budget decision. A missing `recorded_at` (no `created_at` on the
/// recording comment) imposes no wait: deferring on it could defer forever,
/// and the budget still bounds the pushes.
#[must_use]
pub fn decide_budget(
    pos: &ChainPosition,
    cfg: &BudgetConfig,
    now: DateTime<Utc>,
) -> BudgetDecision {
    if pos.spent >= cfg.budget {
        return BudgetDecision::Exhausted { spent: pos.spent };
    }
    // Both clamps keep this addition far from overflow; if it ever did
    // overflow, pushing (still budget-bounded) beats deferring forever.
    let retry_after = pos
        .recorded_at
        .filter(|_| pos.spent >= 1)
        .and_then(|at| at.checked_add_signed(cfg.backoff_after(pos.spent)));
    match retry_after {
        Some(t) if now < t => BudgetDecision::Defer { retry_after: t },
        _ => BudgetDecision::Push { n: pos.spent + 1 },
    }
}

#[cfg(test)]
#[path = "budget_tests.rs"]
mod tests;
