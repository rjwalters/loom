//! Stall signals (#10232, for #10210): the host's forge rate-limit budget,
//! the rate-limit breaker, and the token pool.
//!
//! All three are host-wide, so one [`StallSnapshot`] per ETA pass serves
//! every item. Collecting it makes **no forge call**: the budget comes from
//! the forge-call sink's free `x-ratelimit-*` header readings (and the
//! breaker's own probe, when newer), the breaker from its in-process state,
//! the pool from the token directory.
//!
//! **Identity.** Each reader App installation and the writer own separate
//! rate-limit budgets, and two readers share the `reader` role, so neither
//! the freshest reading nor a role-keyed one says anything about an item's
//! next read. Each item's repo resolves to the reader App that serves it
//! ([`crate::forge_identity::read_credential`]), and only readings carrying
//! that reader's public bucket label ([`crate::forge_identity::reader_bucket`]:
//! App id + owner, never a credential) are used. A repo with no applicable
//! reader (it reads on the writer) is omitted with
//! [`reason::NO_READER_FOR_REPO`]; a reader with no fresh reading with
//! [`reason::NO_IDENTITY_READING`] — never another identity's values.

use super::explanation::{FeatureOmitted, Features};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;
use std::path::Path;

/// A snapshot older than this at `as_of` is not used (three passes).
pub const MAX_AGE_SEC: i64 = 15 * 60;

/// A budget reading older than this when the snapshot is taken is not used.
pub const READING_MAX_AGE_SEC: i64 = 15 * 60;

/// The stall features, in [`Features`] field order.
pub const NAMES: [&str; 8] = [
    "pool_usable_accounts",
    "pool_exhausted",
    "ratelimit_core_remaining",
    "ratelimit_core_reset_at",
    "ratelimit_graphql_remaining",
    "ratelimit_graphql_reset_at",
    "breaker_state",
    "breaker_cooldown_until",
];

/// Omission reasons this module assigns (`features_omitted[].reason`).
pub mod reason {
    /// No snapshot was taken before `as_of` (no pass yet).
    pub const NO_STALL_SNAPSHOT: &str = "no_stall_snapshot";
    /// The last snapshot is older than [`super::MAX_AGE_SEC`].
    pub const STALE_INPUTS: &str = "stale_inputs";
    /// No token pool is provisioned on this host.
    pub const NO_TOKEN_POOL: &str = "no_token_pool";
    /// No reader App serves the repo (its reads run on the writer, whose
    /// budget is not tracked per repo), so no reader budget applies.
    pub const NO_READER_FOR_REPO: &str = "no_reader_for_repo";
    /// No fresh reading attributed to the reader App serving the repo.
    pub const NO_IDENTITY_READING: &str = "no_identity_reading";
    /// The reading carried no reset instant (a breaker probe).
    pub const NO_RESET_IN_READING: &str = "no_reset_in_reading";
    /// No rate-limit breaker is registered in this process.
    pub const BREAKER_NOT_REGISTERED: &str = "breaker_not_registered";
    /// The breaker is closed, so there is no cooldown.
    pub const BREAKER_CLOSED: &str = "breaker_closed";
}

/// One pool's budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetReading {
    /// Calls left.
    pub remaining: u64,
    /// When the pool resets, when the reading said.
    pub reset_at: Option<DateTime<Utc>>,
}

/// The breaker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakerReading {
    /// `closed` or `cooldown`.
    pub state: String,
    /// When an active cooldown releases.
    pub cooldown_until: Option<DateTime<Utc>>,
}

/// The token pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolReading {
    /// Accounts a spawn could select.
    pub usable: usize,
    /// Accounts provisioned.
    pub total: usize,
}

/// The budgets of the reader App serving one repo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoBudget {
    /// The REST (`core`) budget.
    pub core: Option<BudgetReading>,
    /// The GraphQL budget.
    pub graphql: Option<BudgetReading>,
}

/// The host's stall signals at `observed_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StallSnapshot {
    /// When it was taken.
    pub observed_at: DateTime<Utc>,
    /// The serving reader's budgets per (lowercased) `owner/repo`. A repo
    /// with no applicable reader is absent.
    pub budgets: BTreeMap<String, RepoBudget>,
    /// The breaker, when one is registered.
    pub breaker: Option<BreakerReading>,
    /// The pool `workspace_root` resolves to.
    pub pool: PoolReading,
}

/// The reading of `pool` in `budget`, when it was observed at most
/// [`READING_MAX_AGE_SEC`] before `now`.
#[must_use]
pub fn reading(
    budget: &[crate::types::ForgeBudgetReading],
    pool: &str,
    now: DateTime<Utc>,
) -> Option<BudgetReading> {
    budget
        .iter()
        .find(|r| r.pool == pool && now - r.observed_at <= Duration::seconds(READING_MAX_AGE_SEC))
        .map(|r| BudgetReading {
            remaining: r.remaining,
            reset_at: r.reset_at,
        })
}

/// Take the snapshot for `workspace_root` at `now`, with the budgets of the
/// reader App serving each of `repos` (lowercased `owner/repo`, as the feature
/// reads address them). Local reads only: the forge-call sink, the breaker's
/// in-process state, the token directory.
#[must_use]
pub fn collect(workspace_root: &Path, repos: &[String], now: DateTime<Utc>) -> StallSnapshot {
    let breaker = crate::rate_limit_breaker::global().map(|b| b.snapshot(now));
    let buckets = crate::forge_call_stats::bucket_readings(now);
    let budgets = repo_budgets(
        repos,
        |repo| {
            crate::forge_identity::read_credential(repo, None)
                .map(|(_, app_id)| crate::forge_identity::reader_bucket(&app_id, repo))
        },
        &buckets,
        now,
    );
    let pool = crate::tokens_pool::select::spawnable_pool_state(workspace_root);
    StallSnapshot {
        observed_at: now,
        budgets,
        breaker: breaker.map(|b| BreakerReading {
            state: b.phase.as_str().to_string(),
            cooldown_until: b.cooldown_until,
        }),
        pool: PoolReading {
            usable: pool.usable,
            total: pool.total,
        },
    }
}

/// The budgets of each repo's serving reader: `bucket_of` names the reader
/// bucket serving a repo (`None` = no reader), `buckets` holds the readings
/// per bucket. A repo whose reader has no fresh reading maps to an empty
/// [`RepoBudget`]; one with no reader is left out.
#[must_use]
pub fn repo_budgets(
    repos: &[String],
    bucket_of: impl Fn(&str) -> Option<String>,
    buckets: &BTreeMap<String, Vec<crate::types::ForgeBudgetReading>>,
    now: DateTime<Utc>,
) -> BTreeMap<String, RepoBudget> {
    repos
        .iter()
        .filter_map(|repo| {
            let readings = buckets
                .get(&bucket_of(repo)?)
                .map(Vec::as_slice)
                .unwrap_or_default();
            Some((
                repo.clone(),
                RepoBudget {
                    core: reading(readings, "core", now),
                    graphql: reading(readings, "graphql", now),
                },
            ))
        })
        .collect()
}

fn omit(omitted: &mut Vec<FeatureOmitted>, name: &str, why: &str) {
    omitted.push(FeatureOmitted {
        name: name.to_string(),
        reason: why.to_string(),
    });
}

fn budget_to(
    reading: Option<&Option<BudgetReading>>,
    names: [&str; 2],
    remaining: &mut Option<u32>,
    reset_at: &mut Option<DateTime<Utc>>,
    omitted: &mut Vec<FeatureOmitted>,
) {
    let Some(r) = reading.and_then(Option::as_ref) else {
        let why = if reading.is_some() {
            reason::NO_IDENTITY_READING
        } else {
            reason::NO_READER_FOR_REPO
        };
        for name in names {
            omit(omitted, name, why);
        }
        return;
    };
    *remaining = Some(u32::try_from(r.remaining).unwrap_or(u32::MAX));
    *reset_at = r.reset_at;
    if r.reset_at.is_none() {
        omit(omitted, names[1], reason::NO_RESET_IN_READING);
    }
}

/// Write the stall features of `snapshot` at `as_of`, and a reason for each
/// one left null. A snapshot taken at or after `as_of` is not used.
pub fn write_to(
    snapshot: Option<&StallSnapshot>,
    repo: &str,
    as_of: DateTime<Utc>,
    features: &mut Features,
    omitted: &mut Vec<FeatureOmitted>,
) {
    let snap = match snapshot {
        Some(s) if s.observed_at < as_of => s,
        _ => {
            for name in NAMES {
                omit(omitted, name, reason::NO_STALL_SNAPSHOT);
            }
            return;
        }
    };
    if as_of - snap.observed_at > Duration::seconds(MAX_AGE_SEC) {
        for name in NAMES {
            omit(omitted, name, reason::STALE_INPUTS);
        }
        return;
    }
    if snap.pool.total == 0 {
        omit(omitted, "pool_usable_accounts", reason::NO_TOKEN_POOL);
        omit(omitted, "pool_exhausted", reason::NO_TOKEN_POOL);
    } else {
        features.pool_usable_accounts = Some(u32::try_from(snap.pool.usable).unwrap_or(u32::MAX));
        features.pool_exhausted = Some(snap.pool.usable == 0);
    }
    let budget = snap.budgets.get(repo);
    budget_to(
        budget.map(|b| &b.core),
        ["ratelimit_core_remaining", "ratelimit_core_reset_at"],
        &mut features.ratelimit_core_remaining,
        &mut features.ratelimit_core_reset_at,
        omitted,
    );
    budget_to(
        budget.map(|b| &b.graphql),
        ["ratelimit_graphql_remaining", "ratelimit_graphql_reset_at"],
        &mut features.ratelimit_graphql_remaining,
        &mut features.ratelimit_graphql_reset_at,
        omitted,
    );
    match &snap.breaker {
        None => {
            omit(omitted, "breaker_state", reason::BREAKER_NOT_REGISTERED);
            omit(omitted, "breaker_cooldown_until", reason::BREAKER_NOT_REGISTERED);
        }
        Some(b) => {
            features.breaker_state = Some(b.state.clone());
            features.breaker_cooldown_until = b.cooldown_until;
            if b.cooldown_until.is_none() {
                omit(omitted, "breaker_cooldown_until", reason::BREAKER_CLOSED);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "stall_features_tests.rs"]
mod tests;
