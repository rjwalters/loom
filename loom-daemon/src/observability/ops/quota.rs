//! Quota burn and pool state (Issue #8857): is each subscription being used
//! around the clock, or sitting idle, or starved because every account in its
//! pool is exhausted?
//!
//! Sampled on the collector's snapshot cadence and exported through
//! `metric.points` (OTLP-only). Two signal families:
//!
//! # Token burn — `loom.llm.tokens.*`, `loom.llm.requests`
//!
//! Delta counters labelled `provider` + `model`, covering the interval since
//! the previous sample; TPM/RPM are rates over them. Host-wide, not per run:
//! a quota is spent by every session on the host (dispatched, role tick or
//! interactive), so every session counts. Per-execution attribution is #8908.
//!
//! Every subscription store on the host is read incrementally through one
//! seam ([`burn`]): Claude transcripts ([`claude`]), Codex rollouts
//! ([`codex`]), OpenCode's SQLite store, which carries the Z.ai GLM plan
//! ([`opencode`]), and Kimi wire logs ([`kimi`]). Each poll reads only what was
//! written since the previous one ([`tail`] keeps a byte cursor per file), and
//! each event is counted once, in the window its own timestamp falls in.
//! Windows end a settle lag before the sample, abut, and are exported with
//! that end as the point time. The first sample only anchors the window:
//! history is never replayed as a burst.
//!
//! # Pool state — `loom.pool.*`
//!
//! Per provider, from the accounts `tokens.snapshot` already sampled (the
//! Claude `.ranking` pool and the Codex account registry) plus every enabled
//! API-key-pool account (Z.ai GLM, Kimi, …):
//!
//! - `loom.pool.accounts{state=usable|exhausted}` gauges.
//! - `loom.pool.exhausted` gauge: `1` when the pool has an exhausted account
//!   and no usable one — dispatch against that provider is starved.
//! - `loom.pool.exhaustions` delta: accounts that newly read exhausted since
//!   the previous sample (frequency of backoffs).
//! - `loom.pool.exhausted_seconds` delta: the interval since the previous
//!   sample, credited when the pool read exhausted at that previous sample
//!   (sample-and-hold). Summed over a day, this is the downtime.
//!
//! Those four stay per-provider aggregates with no account label. Per-account
//! state (Issue #11286) is its own family, so no existing query changes shape:
//!
//! - `loom.pool.account_state{provider,account,state=usable|exhausted}`: one
//!   0/1 gauge per account and state, every sample — a fixed two series per
//!   registered account.
//! - `loom.pool.plan_token_limit{provider,account}` and
//!   `loom.pool.plan_window_seconds{provider,account}`: the plan allowance and
//!   window an operator declared for an API-key seat (`api-keys limit
//!   --plan-token-limit / --exhaustion-window`). Emitted only where declared —
//!   never a zero-filled denominator. They are the raw facts a Z.ai
//!   utilization figure needs (`loom.llm.tokens.*{provider=zai}` over the
//!   window, divided by the summed limits); the dashboard does that division,
//!   the daemon does not.
//!
//! Account names are the non-secret pool names operators chose
//! (`.loom/api-keys/<provider>/<name>.env`, `.ranking` names), the same
//! identity `github.ratelimit.*` already labels with.

pub mod burn;
pub mod claude;
pub mod codex;
pub mod kimi;
pub mod opencode;
pub mod tail;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, Utc};

use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::telemetry::TokenAccountState;
pub use burn::{burn_points, Burn, ModelBurn, MESSAGE_SETTLE_LAG_SECS};
pub use claude::CLAUDE_PROVIDER;

/// One account's standing in its provider's pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolAccount {
    pub provider: String,
    pub account: String,
    pub usable: bool,
    pub exhausted: bool,
    /// Declared plan token allowance per window (API-key pool only, #11286).
    pub plan_token_limit: Option<u64>,
    /// Declared plan window in seconds (API-key pool only, #11286).
    pub plan_window_secs: Option<u64>,
}

impl PoolAccount {
    /// An account with no declared plan facts.
    #[must_use]
    pub fn new(provider: &str, account: &str, usable: bool, exhausted: bool) -> Self {
        PoolAccount {
            provider: provider.to_string(),
            account: account.to_string(),
            usable,
            exhausted,
            plan_token_limit: None,
            plan_window_secs: None,
        }
    }
}

impl From<&TokenAccountState> for PoolAccount {
    fn from(state: &TokenAccountState) -> Self {
        PoolAccount::new(&state.provider, &state.account, !state.exhausted, state.exhausted)
    }
}

/// Every enabled API-key-pool account (Z.ai, Kimi, …) under `roots`, or
/// `None` when the pool cannot be read: unknown, not empty.
fn api_key_accounts_in(roots: &[PathBuf]) -> Option<Vec<PoolAccount>> {
    use crate::api_keys_pool::Ineligible;
    let accounts = crate::api_keys_pool::select::list_accounts_in(roots, None).ok()?;
    Some(
        accounts
            .into_iter()
            .filter(|account| account.enabled)
            .map(|account| PoolAccount {
                usable: matches!(account.ineligible, None | Some(Ineligible::AtCapacity)),
                exhausted: account.ineligible == Some(Ineligible::Exhausted),
                plan_token_limit: account.plan_token_limit,
                plan_window_secs: account.exhaustion_window_secs,
                provider: account.provider,
                account: account.name,
            })
            .collect(),
    )
}

/// What the previous sample saw, so the next one can emit deltas.
#[derive(Debug, Default)]
pub struct QuotaState {
    /// When the previous pool sample was taken.
    last_pool_sample: Option<DateTime<Utc>>,
    /// Exhausted accounts per provider at the previous sample.
    exhausted: BTreeMap<String, BTreeSet<String>>,
    /// Providers whose pool read exhausted at the previous sample.
    pools_exhausted: BTreeSet<String>,
    /// The last API-key pool read that succeeded.
    api_keys: Vec<PoolAccount>,
}

impl QuotaState {
    /// The API-key pool accounts to sample: this read when it succeeded, else
    /// the last good one. A transient read failure must not make every
    /// account vanish for a tick and then count as newly exhausted on the
    /// next (#8941 item 1).
    pub fn api_key_pool(&mut self, read: Option<Vec<PoolAccount>>) -> Vec<PoolAccount> {
        if let Some(accounts) = read {
            self.api_keys = accounts;
        }
        self.api_keys.clone()
    }

    /// Pool gauges and deltas for `accounts` at `now`, advancing the state.
    /// The delta counters are only emitted once a previous sample exists.
    pub fn pool_points(
        &mut self,
        accounts: &[PoolAccount],
        now: DateTime<Utc>,
    ) -> Vec<MetricPoint> {
        let mut points = account_points(accounts);
        let mut by_provider: BTreeMap<&str, (i64, BTreeSet<String>)> = BTreeMap::new();
        for account in accounts {
            let entry = by_provider.entry(account.provider.as_str()).or_default();
            if account.usable {
                entry.0 += 1;
            }
            if account.exhausted {
                entry.1.insert(account.account.clone());
            }
        }
        let elapsed = self
            .last_pool_sample
            .map(|last| (now - last).num_seconds().max(0));
        let mut exhausted_now = BTreeMap::new();
        let mut pools_exhausted = BTreeSet::new();
        for (provider, (usable, exhausted)) in by_provider {
            let exhausted_count = i64::try_from(exhausted.len()).unwrap_or(i64::MAX);
            let pool_exhausted = exhausted_count > 0 && usable == 0;
            points.push(
                MetricPoint::int(MetricName::PoolAccounts, usable)
                    .label("provider", provider)
                    .label("state", "usable"),
            );
            points.push(
                MetricPoint::int(MetricName::PoolAccounts, exhausted_count)
                    .label("provider", provider)
                    .label("state", "exhausted"),
            );
            points.push(
                MetricPoint::int(MetricName::PoolExhausted, i64::from(pool_exhausted))
                    .label("provider", provider),
            );
            if let Some(elapsed) = elapsed {
                let previous = self.exhausted.get(provider);
                let newly = exhausted
                    .iter()
                    .filter(|name| previous.is_none_or(|prev| !prev.contains(*name)))
                    .count();
                if newly > 0 {
                    points.push(
                        MetricPoint::int(
                            MetricName::PoolExhaustions,
                            i64::try_from(newly).unwrap_or(i64::MAX),
                        )
                        .label("provider", provider),
                    );
                }
                if elapsed > 0 && self.pools_exhausted.contains(provider) {
                    points.push(
                        MetricPoint::int(MetricName::PoolExhaustedSeconds, elapsed)
                            .label("provider", provider),
                    );
                }
            }
            if pool_exhausted {
                pools_exhausted.insert(provider.to_string());
            }
            exhausted_now.insert(provider.to_string(), exhausted);
        }
        self.exhausted = exhausted_now;
        self.pools_exhausted = pools_exhausted;
        self.last_pool_sample = Some(now);
        points
    }
}

/// Per-account gauges (Issue #11286): `loom.pool.account_state` for every
/// account and state, plus the declared plan facts where present. The same
/// account seen twice (a name in two pools of one provider) is emitted once.
fn account_points(accounts: &[PoolAccount]) -> Vec<MetricPoint> {
    let mut seen = BTreeSet::new();
    let mut points = Vec::new();
    for account in accounts {
        if !seen.insert((account.provider.as_str(), account.account.as_str())) {
            continue;
        }
        let labelled = |point: MetricPoint| {
            point
                .label("provider", account.provider.as_str())
                .label("account", account.account.as_str())
        };
        for (state, on) in [("usable", account.usable), ("exhausted", account.exhausted)] {
            points.push(
                labelled(MetricPoint::int(MetricName::PoolAccountState, i64::from(on)))
                    .label("state", state),
            );
        }
        if let Some(limit) = account.plan_token_limit {
            points.push(labelled(MetricPoint::int(
                MetricName::PoolPlanTokenLimit,
                i64::try_from(limit).unwrap_or(i64::MAX),
            )));
        }
        if let Some(window) = account.plan_window_secs {
            points.push(labelled(MetricPoint::int(
                MetricName::PoolPlanWindowSeconds,
                i64::try_from(window).unwrap_or(i64::MAX),
            )));
        }
    }
    points
}

static STATE: Mutex<Option<QuotaState>> = Mutex::new(None);
static BURN: Mutex<Option<Burn>> = Mutex::new(None);

/// One sample's two batches and the interval each one's deltas cover.
struct Sample {
    burn: Option<(DateTime<Utc>, DateTime<Utc>, Vec<MetricPoint>)>,
    pool: Vec<MetricPoint>,
    pool_start: Option<DateTime<Utc>>,
}

fn sample(workspace_root: &Path, mut accounts: Vec<PoolAccount>, now: DateTime<Utc>) -> Sample {
    let api_keys = api_key_accounts_in(&crate::api_keys_pool::paths::pool_roots(workspace_root));
    let (pool, pool_start) = {
        let mut guard = STATE.lock().unwrap_or_else(PoisonError::into_inner);
        let state = guard.get_or_insert_with(QuotaState::default);
        accounts.extend(state.api_key_pool(api_keys));
        let pool_start = state.last_pool_sample;
        (state.pool_points(&accounts, now), pool_start)
    };
    let burn = BURN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(Burn::host)
        .sample(now)
        .map(|(start, end, burn)| (start, end, burn_points(&burn)));
    Sample {
        burn,
        pool,
        pool_start,
    }
}

/// Sample and export, when an ops sink is registered. `accounts` is the
/// `tokens.snapshot` account list the collector already built this tick.
/// Returns before any store is read when no OTLP exporter is running.
pub async fn record(workspace_root: &Path, accounts: &[TokenAccountState]) {
    let Some(sink) = super::global_ops_sink() else {
        return;
    };
    let root = workspace_root.to_path_buf();
    let accounts: Vec<PoolAccount> = accounts.iter().map(PoolAccount::from).collect();
    let now = Utc::now();
    if let Ok(sample) = tokio::task::spawn_blocking(move || sample(&root, accounts, now)).await {
        if let Some((start, end, points)) = sample.burn {
            // Stamped at the window end, so consecutive intervals abut.
            sink.emit_metrics_over(points, Some(start), end);
        }
        sink.emit_metrics_since(sample.pool, sample.pool_start);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "quota_tests.rs"]
mod tests;
