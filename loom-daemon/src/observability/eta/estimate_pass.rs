//! One estimate pass under the tracker lock (split out of `eta.rs`, #10528).

use super::{current_ids, shadow, State};
use crate::eta::planner_version::Stamped as _;
use crate::eta::stall::{self, StallSnapshot};
use crate::eta::tracker::{Emission, EstimateContext, ItemKey};
use chrono::{DateTime, Utc};

/// Estimate `keys` (all when `None`) under the lock, returning the
/// emissions plus what delivery needs.
pub(super) fn estimate_locked(
    state: &mut State,
    keys: Option<&[ItemKey]>,
    now: DateTime<Utc>,
) -> Vec<Emission> {
    let stalls = stalls_now(state, now);
    let ctx = EstimateContext {
        registry: &state.registry,
        current_start: state.config.current_start.as_deref(),
        current_finish: state.config.current_finish.as_deref(),
        current_land: state.config.current_land.as_deref(),
        history: &state.history,
        refresh_secs: state.config.refresh_secs,
        host_id: Some(state.host_id.as_str()),
        repo_ids: &state.repo_ids,
        stalls: &stalls,
    };
    let emissions = state
        .tracker
        .estimate(keys, &ctx, now)
        .stamped(&state.workspace_root);
    // A full pass also tallied every live series' answer state (#10233):
    // fold it into the ledger as paired answer-rate evidence.
    let answers = state.tracker.drain_answers();
    if !answers.is_empty() {
        let ids = current_ids(state);
        state
            .shadow
            .record_answers(&|kind| ids.get(&kind).cloned().unwrap_or_default(), &answers);
        let path = shadow::ledger_path(&state.workspace_root);
        if let Err(error) = shadow::write_ledger(&path, &state.shadow) {
            log::warn!("eta: persisting the shadow ledger failed: {error}");
        }
    }
    emissions
}

/// The stall snapshot an estimate at `now` reads (#10210): the rate-limit
/// breaker and the forge-call ledger's last zero readings (in-process, read
/// fresh), plus the pass's token-pool brake and lockout readings. Read-only:
/// no stall costs a forge call.
fn stalls_now(state: &State, now: DateTime<Utc>) -> StallSnapshot {
    let breaker = crate::rate_limit_breaker::global_snapshot();
    let pools = crate::forge_call_stats::exhausted_pools(now);
    let pools: Vec<(&str, Option<DateTime<Utc>>)> = pools
        .iter()
        .map(|(pool, reset)| (pool.as_str(), *reset))
        .collect();
    StallSnapshot {
        host: stall::host_signals(breaker.as_ref(), &pools, state.pool_exhausted, now),
        locked_repos: state.locked_repos.clone(),
    }
}
