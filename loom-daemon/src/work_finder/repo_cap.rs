//! Per-repo dispatch cap and repo-track affinity (issue #9090).
//!
//! The multi-workspace tick has always filled ONE global budget
//! (`max_concurrent`) in one globally-sorted order, so nothing stopped the
//! whole budget landing in a single repo: N sweeps in one repo rebase against
//! each other's merges, while sibling repos sat idle behind them. This module
//! adds the two pieces that bound that, both at the same layer #6243's
//! repo-sharding partition already occupies — the already-sorted candidate
//! list, between the global sort and pass 2's dispatch loop:
//!
//! 1. **Per-repo cap** (`autonomous.workFinder.maxConcurrentPerRepo`) — an
//!    *admission* bound. A candidate whose own repo is already at its cap is
//!    **deferred**, never dropped, and the loop continues to the next
//!    candidate, which by construction is some other repo's work.
//! 2. **Track affinity** — an *ordering* preference. A repo that already has a
//!    live sweep floats ahead of a cold repo, preserving relative order within
//!    each group (a stable partition), so a repo's own backlog drains in
//!    sequence rather than one-issue-per-repo round-robin.
//!
//! Affinity is deliberately **subordinate** to the cap: it decides order, the
//! cap decides admission. The reverse (affinity over admission) would let a hot
//! repo keep taking slots, which is exactly what the cap exists to stop.
//!
//! # Off by default
//!
//! `maxConcurrentPerRepo` absent ⇒ [`RepoCap::disabled`]-equivalent: the cap
//! never defers, affinity never reorders, and the candidate list pass 2 sees is
//! byte-for-byte the pre-#9090 one. A non-`None` default would silently
//! throttle every fleet on upgrade, so this follows `extraSkipLabels` /
//! `preferred_slice` and is opt-in. Setting the key enables **both** halves:
//! affinity without the cap is a hot-repo preference with no bound, which is a
//! regression, not a feature.
//!
//! # Work conservation
//!
//! Structural, not a special case: a cap deferral consumes no slot and pass 2
//! moves straight on to the next candidate, so a capped repo's deferral is
//! always usable by another repo's candidate **in the same tick** — including
//! a cold, lower-priority repo that affinity had floated to the back. The one
//! case where a free global slot is deliberately left unused is when EVERY
//! remaining candidate's repo is at its own cap; that is the cap doing its job,
//! identical in shape to `occupancy >= max_concurrent` idling a tick.
//!
//! # Not a cross-repo CI interleaving mechanism
//!
//! The third behaviour #9090 asked for — "interleave to repo B/C while repo A
//! waits on CI" — is **already emergent** and deliberately gets no mechanism
//! here: the #4123 open-PR guard makes an issue whose PR is in flight a
//! non-candidate (`OpenPrDispatchError` ⇒ `Qd::OpenPr` / `skipped_pr_open`),
//! and pass 2 continues to the next candidate in global order, which is some
//! other repo's work. Re-engagement on merge is automatic (the guard stops
//! refusing). `repo_cap_tests.rs` pins that behaviour rather than duplicating
//! it.

use serde_json::Value;

use super::{
    ready_queue, PriorityCandidate, TickReport, WorkDispatcher, WorkFinderConfig, WorkSource,
};
use crate::types::QueueDisposition as Qd;

/// Env override for the per-repo dispatch cap (#9090) — `None` when unset,
/// zero, or unparseable.
///
/// Zero is treated as **absent**, never as a cap of `0`: a literal zero cap
/// would defer every candidate in every repo forever, i.e. deadlock the fleet
/// while looking configured. Same contract as
/// [`WORK_FINDER_MAX_CONCURRENT_ENV`](super::WORK_FINDER_MAX_CONCURRENT_ENV).
pub const WORK_FINDER_MAX_CONCURRENT_PER_REPO_ENV: &str =
    "LOOM_WORK_FINDER_MAX_CONCURRENT_PER_REPO";

/// Parse `autonomous.workFinder.maxConcurrentPerRepo` out of the `workFinder`
/// sub-block, soft-failing to `None` (uncapped) on absent, non-integer, or
/// zero — the same contract `maxConcurrent` uses, with `None` meaning "no
/// per-repo bound" rather than "fall through to a default".
#[must_use]
pub fn parse_config(wf: Option<&Value>) -> Option<usize> {
    wf.and_then(|w| w.get("maxConcurrentPerRepo"))
        .and_then(Value::as_u64)
        .filter(|&n| n > 0)
        .and_then(|n| usize::try_from(n).ok())
}

/// Env override, `None` when unset/zero/unparseable.
fn env_max_concurrent_per_repo() -> Option<usize> {
    std::env::var(WORK_FINDER_MAX_CONCURRENT_PER_REPO_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
}

/// Resolve the per-repo cap with precedence **env > config > none**.
///
/// Unlike every other work-finder knob there is no built-in default to fall
/// through to: absent at both layers means *uncapped*, the pre-#9090
/// behaviour. Re-resolved every tick through
/// [`ConfiguredMaxReloader`](super::ConfiguredMaxReloader), so a config edit
/// hot-applies exactly as `maxConcurrent` does since #9060.
#[must_use]
pub fn resolve(config: &WorkFinderConfig) -> Option<usize> {
    env_max_concurrent_per_repo().or(config.max_concurrent_per_repo)
}

/// Pass 2's per-repo admission counter: the cap plus one live occupancy count
/// per workspace, seeded from each dispatcher's **own**
/// [`WorkDispatcher::occupancy`] — already per-repo, and already the value the
/// tick sums for its global seed, so this reads no new forge/registry state.
#[derive(Debug, Clone)]
pub struct RepoCap {
    /// `None` ⇒ uncapped (and affinity off).
    cap: Option<usize>,
    /// Live per-workspace occupancy, incremented per admission this tick.
    occupancy: Vec<usize>,
    /// Which workspaces had a live sweep at the **top** of the tick — the
    /// affinity "hot track" set. Snapshotted, not live, so the ordering a tick
    /// computes cannot shift underneath its own dispatch loop.
    hot: Vec<bool>,
}

impl RepoCap {
    /// Seed from this tick's per-workspace occupancy.
    ///
    /// A `Some(0)` cap is defensively coerced to uncapped (see
    /// [`WORK_FINDER_MAX_CONCURRENT_PER_REPO_ENV`]) — the parsers already drop
    /// zero, so this only guards a hand-constructed value.
    #[must_use]
    pub fn new(cap: Option<usize>, occupancy: Vec<usize>) -> Self {
        Self {
            cap: cap.filter(|&c| c > 0),
            hot: occupancy.iter().map(|&o| o > 0).collect(),
            occupancy,
        }
    }

    /// An uncapped, affinity-off state — the value every non-multi-workspace
    /// and non-opted-in path uses.
    #[must_use]
    pub fn disabled() -> Self {
        Self::new(None, Vec::new())
    }

    /// Whether a per-repo cap is in force this tick (and therefore whether
    /// affinity reorders).
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.cap.is_some()
    }

    /// Whether workspace `idx` already has a live sweep as of the top of this
    /// tick — affinity's "same track" predicate. A missing entry is cold.
    #[must_use]
    fn is_hot(&self, idx: usize) -> bool {
        self.hot.get(idx).copied().unwrap_or(false)
    }

    /// Whether workspace `idx` is at (or over) its cap right now. A missing
    /// occupancy entry counts as zero — fail open toward dispatching, never
    /// toward stranding a workspace the caller forgot to seed.
    #[must_use]
    fn at_cap(&self, idx: usize) -> bool {
        self.cap
            .is_some_and(|cap| self.occupancy.get(idx).copied().unwrap_or(0) >= cap)
    }

    /// Defer `cand` when its repo is at its cap, recording the deferral on
    /// `report` (counter + ready-queue row) so it is visible instead of
    /// silently vanishing. `true` ⇒ the caller must skip this candidate.
    ///
    /// Purely observational, exactly like `deferred_out_of_slice`: the
    /// candidate is not lost — it stays ready and is re-evaluated next tick.
    pub fn defer(&self, cand: &PriorityCandidate, report: &mut TickReport) -> bool {
        if !self.at_cap(cand.workspace_idx) {
            return false;
        }
        report.deferred_repo_cap += 1;
        let detail = self.cap.map(|cap| format!("repo at its cap of {cap}"));
        ready_queue::resolve(&mut report.queue, cand, Qd::DeferredRepoCap, detail);
        true
    }

    /// Count one successful dispatch against workspace `idx`'s cap — the
    /// per-repo twin of pass 2's `occupancy += 1`.
    pub fn admit(&mut self, idx: usize) {
        if let Some(slot) = self.occupancy.get_mut(idx) {
            *slot += 1;
        }
    }
}

/// Shape the globally-sorted candidate list for pass 2: the #6243 repo-sharding
/// slice partition, then (when a per-repo cap is configured) the #9090 track
/// affinity partition.
///
/// Both are **stable partitions** of an already-sorted list, so
/// [`candidate_cmp`](super::candidate_cmp) still decides order within each
/// group and the comparator itself is untouched — its ordering tests stay
/// valid. With `preferred_slice: None` and a disabled `cap` this returns
/// `candidates` unchanged.
#[must_use]
pub fn shape_queue(
    candidates: Vec<PriorityCandidate>,
    preferred_slice: Option<&[bool]>,
    cap: &RepoCap,
    report: &mut TickReport,
) -> Vec<PriorityCandidate> {
    let sliced = apply_slice(candidates, preferred_slice, report);
    if !cap.enabled() {
        return sliced;
    }
    // Track affinity (#9090): float repos that already have a live sweep ahead
    // of cold ones. `Iterator::partition` preserves relative order within both
    // halves, so this is a stable reordering of the sorted list — not a new
    // comparator. Ordering only: a floated candidate still has to pass the
    // per-repo cap in pass 2, and a deferred one costs the cold group nothing
    // (pass 2 continues to it in the same tick).
    let (hot, cold): (Vec<_>, Vec<_>) = sliced
        .into_iter()
        .partition(|c| cap.is_hot(c.workspace_idx));
    hot.into_iter().chain(cold).collect()
}

/// The #6243 repo-sharding slice partition, moved here verbatim from
/// `tick_multi_with_sharding` (#9090) so its sibling affinity pass sits next to
/// it and `work_finder.rs` — frozen by the file-size ratchet — does not grow.
///
/// `None` is a no-op. Otherwise the already-sorted list splits into in-slice /
/// out-of-slice preserving each partition's relative order; out-of-slice
/// candidates dispatch THIS TICK only when the slice was completely empty at
/// the top of the tick (work-conservation, #6243's AC).
fn apply_slice(
    candidates: Vec<PriorityCandidate>,
    preferred_slice: Option<&[bool]>,
    report: &mut TickReport,
) -> Vec<PriorityCandidate> {
    let Some(slice) = preferred_slice else {
        return candidates;
    };
    let (in_slice, out_of_slice): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|c| slice.get(c.workspace_idx).copied().unwrap_or(true));
    if in_slice.is_empty() {
        // This host's slice has zero eligible candidates this tick — fall back
        // to the full out-of-slice queue rather than starving while other
        // repos have ready work.
        return out_of_slice;
    }
    report.deferred_out_of_slice += out_of_slice.len();
    for c in &out_of_slice {
        ready_queue::resolve(&mut report.queue, c, Qd::DeferredOutOfSlice, None);
    }
    in_slice
}

/// Like [`tick_multi_with_repo_cap`](super::tick_multi_with_repo_cap) with no
/// per-repo cap — the pre-#9090 seven-argument entry point, kept so every
/// existing caller (and every #6243 sharding test) is unchanged.
pub fn tick_multi_with_sharding<S: WorkSource, D: WorkDispatcher>(
    workspaces: &mut [(S, D)],
    priorities: &[u32],
    max_concurrent: usize,
    halted: &[bool],
    max_admissions_per_tick: usize,
    saturation_held: bool,
    preferred_slice: Option<&[bool]>,
) -> TickReport {
    super::tick_multi_with_repo_cap(
        workspaces,
        priorities,
        max_concurrent,
        halted,
        max_admissions_per_tick,
        saturation_held,
        preferred_slice,
        None,
    )
}

#[cfg(test)]
#[path = "repo_cap_tests.rs"]
mod tests;
