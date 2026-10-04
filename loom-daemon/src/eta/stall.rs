//! Stalls (#10210): **stopped** service, as opposed to slow service.
//!
//! The worst ETA misses are not slow stages but stages that are not being
//! served at all: the forge's REST/GraphQL quota is spent, the shared
//! rate-limit breaker is cooling down, the agent token pool has no usable
//! account, an operator holds the PR, or the repo's ready backlog is frozen
//! behind the open-PR guard (`pr-open-skip`). A duration grid learned from
//! served stages says nothing about any of them.
//!
//! # A "not before T" term
//!
//! A stall-aware heuristic (`land-v4`) models the remaining time as
//! **stall term + normal term**: the item is not served before the stall
//! resumes, and is then served as usual. The stall term is
//!
//! - **`resume_at − as_of`** when the stall has a known resume instant (the
//!   quota's reset, the breaker's cooldown end) — deterministic, and
//! - a **documented per-cause default** ([`default_term_sec`]) when it does
//!   not, until a learned stall-duration distribution exists (the operator
//!   hold's response-time model is #10218).
//!
//! The stall pauses the stage clock: the normal term is conditioned on the
//! age the item has *now*, because a stopped stage does not progress.
//!
//! # Observation, not inference
//!
//! Every signal is read-only and already held by the daemon (the breaker's
//! snapshot, the forge-call ledger's last header reading, the sweep
//! registry's empty-pool brake, the lockout clock, the item's own labels):
//! no stall costs a forge call. The daemon assembles them into a
//! [`StallSnapshot`] outside the estimator; the estimator stays pure and only
//! ever sees the [`StallSignal`]s that apply to the item
//! ([`StallSnapshot::for_item`]).
//!
//! Every heuristic **records** the binding stall in `explanation.stalled`
//! (so the dashboard can say "paused for the API quota until 14:00 UTC"),
//! but only a stall-aware one adds the term to its numbers
//! (`stalled.applied`).

use super::labels::operator_hold_label;
use super::Stage;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

/// Why an item is not being served right now. A closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StallCause {
    /// A forge rate-limit pool (`core`, `graphql`) is at zero remaining.
    RateLimitQuota,
    /// The shared rate-limit breaker is in cooldown (forge polling and
    /// dispatch suppressed) without a pool reading at zero.
    BreakerCooldown,
    /// The agent token pool has no spawnable account (the empty-pool brake).
    TokenPoolExhausted,
    /// An operator hold label (`loom:operator`, `-only`, `-decision`,
    /// `-mechanical`) is on the item.
    OperatorHold,
    /// The repo's ready backlog is frozen behind the open-PR guard
    /// (`pr-open-skip`); applies to a ready (`ready_wait`) item only.
    PrOpenLockout,
}

impl StallCause {
    /// Every cause, in wire order.
    pub const ALL: [StallCause; 5] = [
        StallCause::RateLimitQuota,
        StallCause::BreakerCooldown,
        StallCause::TokenPoolExhausted,
        StallCause::OperatorHold,
        StallCause::PrOpenLockout,
    ];

    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            StallCause::RateLimitQuota => "rate_limit_quota",
            StallCause::BreakerCooldown => "breaker_cooldown",
            StallCause::TokenPoolExhausted => "token_pool_exhausted",
            StallCause::OperatorHold => "operator_hold",
            StallCause::PrOpenLockout => "pr_open_lockout",
        }
    }
}

impl fmt::Display for StallCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Default stall term when a quota stall has no known reset: GitHub's primary
/// rate-limit windows are one hour.
pub const DEFAULT_QUOTA_SEC: i64 = 3600;

/// Default stall term for a breaker cooldown with no recorded end: the
/// breaker's own fallback cooldown order of magnitude (it always records an
/// end in practice; this is the floor of a missing one).
pub const DEFAULT_BREAKER_SEC: i64 = 900;

/// Default stall term for an exhausted token pool: one agent usage window
/// (five hours), the time a spent account takes to come back.
pub const DEFAULT_POOL_SEC: i64 = 5 * 3600;

/// Default stall term for an operator hold: one day. A placeholder, not a
/// model — a held PR is still held after a day about a third of the time
/// (#10193 v2); the response-time model replacing this is #10218.
pub const DEFAULT_OPERATOR_HOLD_SEC: i64 = 24 * 3600;

/// Default stall term for a `pr-open-skip` lockout: the lock clears when
/// the repo's open PR lands; four hours is a prior, not a measurement.
pub const DEFAULT_LOCKOUT_SEC: i64 = 4 * 3600;

/// The documented stall term for `cause` when its resume instant is unknown.
#[must_use]
pub fn default_term_sec(cause: StallCause) -> i64 {
    match cause {
        StallCause::RateLimitQuota => DEFAULT_QUOTA_SEC,
        StallCause::BreakerCooldown => DEFAULT_BREAKER_SEC,
        StallCause::TokenPoolExhausted => DEFAULT_POOL_SEC,
        StallCause::OperatorHold => DEFAULT_OPERATOR_HOLD_SEC,
        StallCause::PrOpenLockout => DEFAULT_LOCKOUT_SEC,
    }
}

/// `stalled.term_basis` when the term is `resume_at − as_of`.
pub const TERM_BASIS_RESUME_AT: &str = "resume_at";

/// `stalled.term_basis` when the term is [`default_term_sec`].
pub const TERM_BASIS_DEFAULT: &str = "default";

/// One active stall signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StallSignal {
    /// Why.
    pub cause: StallCause,
    /// When service resumes, when the source knows it.
    pub resume_at: Option<DateTime<Utc>>,
    /// A short machine-readable qualifier (the pool, the hold label, the
    /// breaker's tripping source). Never free text from the forge.
    pub detail: Option<String>,
}

impl StallSignal {
    /// A signal with no detail.
    #[must_use]
    pub fn new(cause: StallCause, resume_at: Option<DateTime<Utc>>) -> Self {
        StallSignal {
            cause,
            resume_at,
            detail: None,
        }
    }

    /// With `detail`.
    #[must_use]
    pub fn with_detail(mut self, detail: &str) -> Self {
        self.detail = Some(detail.to_string());
        self
    }
}

/// Every stall the daemon observed at one instant, before it is narrowed to
/// one item.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StallSnapshot {
    /// Host-wide stalls (quota, breaker, token pool): they stop every item.
    pub host: Vec<StallSignal>,
    /// Lowercased `owner/repo` slugs whose ready backlog is frozen by the
    /// open-PR guard.
    pub locked_repos: BTreeSet<String>,
}

impl StallSnapshot {
    /// The signals that stop `repo`'s item in `stage` (when it has one) with
    /// `labels`: every host-wide signal, the repo's lockout when the item is
    /// still waiting to be dispatched, and an operator hold on the item
    /// itself.
    #[must_use]
    pub fn for_item(
        &self,
        repo: &str,
        stage: Option<Stage>,
        labels: &[String],
    ) -> Vec<StallSignal> {
        let mut signals = self.host.clone();
        if stage == Some(Stage::ReadyWait) && self.locked_repos.contains(&repo.to_ascii_lowercase())
        {
            signals.push(StallSignal::new(StallCause::PrOpenLockout, None));
        }
        if let Some(label) = operator_hold_label(labels) {
            signals.push(StallSignal::new(StallCause::OperatorHold, None).with_detail(label));
        }
        signals
    }
}

/// The binding stall, as an explanation records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stalled {
    /// The binding cause: the one with the longest term.
    pub cause: StallCause,
    /// When it resumes, when known.
    pub resume_at: Option<DateTime<Utc>>,
    /// Seconds from `as_of` until service resumes: `resume_at − as_of`
    /// (floored at zero), or the cause's documented default.
    pub term_sec: i64,
    /// `resume_at` or `default`: where `term_sec` came from.
    pub term_basis: String,
    /// Whether the heuristic added `term_sec` to its result. `false` for
    /// every heuristic that only records the stall, and on a refusal.
    pub applied: bool,
    /// The binding signal's qualifier, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The other active causes, binding first excluded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also: Vec<StallCause>,
}

/// The term `signal` implies at `as_of`, and its basis.
#[must_use]
pub fn term_of(signal: &StallSignal, as_of: DateTime<Utc>) -> (i64, &'static str) {
    match signal.resume_at {
        Some(at) => ((at - as_of).num_seconds().max(0), TERM_BASIS_RESUME_AT),
        None => (default_term_sec(signal.cause), TERM_BASIS_DEFAULT),
    }
}

/// The binding stall among `signals` at `as_of`: the longest term wins (the
/// item is served only when every stall has cleared, and they overlap), a
/// tie goes to the first signal. `None` when nothing stalls the item.
#[must_use]
pub fn binding(signals: &[StallSignal], as_of: DateTime<Utc>) -> Option<Stalled> {
    let mut best: Option<(&StallSignal, i64, &'static str)> = None;
    for signal in signals {
        let (term, basis) = term_of(signal, as_of);
        if best.is_none_or(|(_, t, _)| term > t) {
            best = Some((signal, term, basis));
        }
    }
    let (signal, term_sec, basis) = best?;
    let mut also: Vec<StallCause> = Vec::new();
    for s in signals {
        if s.cause != signal.cause && !also.contains(&s.cause) {
            also.push(s.cause);
        }
    }
    Some(Stalled {
        cause: signal.cause,
        resume_at: signal.resume_at,
        term_sec,
        term_basis: basis.to_string(),
        applied: false,
        detail: signal.detail.clone(),
        also,
    })
}

/// The host-wide stall signals from what the daemon already holds (#10210):
/// the shared rate-limit breaker's snapshot, the pools the forge-call ledger
/// last read at zero (`(pool, reset)`), and whether the agent token pool's
/// empty-pool brake is tripped. Pure.
///
/// - An active breaker cooldown with a pool reading at zero is a
///   `rate_limit_quota` stall, otherwise a `breaker_cooldown` one; both
///   resume at the cooldown's end.
/// - A pool at zero in the ledger is a `rate_limit_quota` stall resuming at
///   its reset — unless the breaker already reported the quota.
/// - The tripped brake is a `token_pool_exhausted` stall with no known
///   resume (its default term applies).
#[must_use]
pub fn host_signals(
    breaker: Option<&crate::rate_limit_breaker::RateLimitSnapshot>,
    pools_at_zero: &[(&str, Option<DateTime<Utc>>)],
    pool_exhausted: bool,
    now: DateTime<Utc>,
) -> Vec<StallSignal> {
    let mut signals = Vec::new();
    if let Some(b) = breaker.filter(|b| b.suppressed) {
        let at_zero = b.core_remaining == Some(0) || b.graphql_remaining == Some(0);
        let cause = if at_zero {
            StallCause::RateLimitQuota
        } else {
            StallCause::BreakerCooldown
        };
        let mut signal = StallSignal::new(cause, b.cooldown_until.filter(|u| *u > now));
        if let Some(source) = &b.source {
            signal = signal.with_detail(source);
        }
        signals.push(signal);
    }
    if !signals
        .iter()
        .any(|s| s.cause == StallCause::RateLimitQuota)
    {
        // The latest reset among the pools at zero: service resumes only
        // when every spent pool has.
        let latest = pools_at_zero
            .iter()
            .max_by_key(|(_, reset)| reset.map_or(i64::MAX, |r| r.timestamp()));
        if let Some((pool, reset)) = latest {
            signals.push(
                StallSignal::new(StallCause::RateLimitQuota, reset.filter(|r| *r > now))
                    .with_detail(pool),
            );
        }
    }
    if pool_exhausted {
        signals.push(StallSignal::new(StallCause::TokenPoolExhausted, None));
    }
    signals
}
