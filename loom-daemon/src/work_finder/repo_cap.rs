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

/// The per-repo admission limits pass 2 enforces: the #9090 per-repo cap and,
/// in production since #11094, the per-repo RAM budget
/// ([`crate::ram_headroom::RamBudget`]). `Option<usize>` converts to a
/// RAM-less value, so every pre-#11094 caller is unchanged.
/// Since #11191 it also carries the per-repo disk budget
/// ([`crate::disk_admission::DiskBudget`]).
#[derive(Debug, Clone, Default)]
pub struct RepoLimits {
    pub per_repo: Option<usize>,
    pub ram: Option<crate::ram_headroom::RamBudget>,
    pub disk: Option<crate::disk_admission::DiskBudget>,
}

impl From<Option<usize>> for RepoLimits {
    fn from(per_repo: Option<usize>) -> Self {
        Self {
            per_repo,
            ..Self::default()
        }
    }
}

impl From<(Option<usize>, Option<crate::ram_headroom::RamBudget>)> for RepoLimits {
    fn from((per_repo, ram): (Option<usize>, Option<crate::ram_headroom::RamBudget>)) -> Self {
        Self {
            per_repo,
            ram,
            disk: None,
        }
    }
}

/// The production tick's limits (#11191): per-repo cap, RAM and disk budgets.
type TickLimits = (
    Option<usize>,
    Option<crate::ram_headroom::RamBudget>,
    Option<crate::disk_admission::DiskBudget>,
);

impl From<TickLimits> for RepoLimits {
    fn from((per_repo, ram, disk): TickLimits) -> Self {
        Self {
            per_repo,
            ram,
            disk,
        }
    }
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
    /// The tick's per-repo RAM budget (#11094); `None` ⇒ no RAM gate here.
    ram: Option<crate::ram_headroom::RamBudget>,
    /// The tick's per-repo disk budget (#11191); `None` ⇒ no disk gate here.
    disk: Option<crate::disk_admission::DiskBudget>,
    /// The same-tick affected-files overlap gate (#9781).
    overlap: super::affected_files::OverlapGate,
}

impl RepoCap {
    /// Seed from this tick's per-workspace occupancy.
    ///
    /// A `Some(0)` cap is defensively coerced to uncapped (see
    /// [`WORK_FINDER_MAX_CONCURRENT_PER_REPO_ENV`]) — the parsers already drop
    /// zero, so this only guards a hand-constructed value.
    #[must_use]
    pub fn new(cap: Option<usize>, occupancy: Vec<usize>) -> Self {
        Self::from_limits(cap.into(), occupancy)
    }

    /// [`Self::new`] plus the tick's per-repo RAM budget (#11094).
    #[must_use]
    pub fn from_limits(limits: RepoLimits, occupancy: Vec<usize>) -> Self {
        Self {
            cap: limits.per_repo.filter(|&c| c > 0),
            hot: occupancy.iter().map(|&o| o > 0).collect(),
            occupancy,
            ram: limits.ram,
            disk: limits.disk,
            overlap: super::affected_files::OverlapGate::default(),
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
    pub(super) fn at_cap(&self, idx: usize) -> bool {
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

    /// Pass 2's per-repo gate (#11094): defer `cand` when its OWN repo's RAM
    /// charge no longer fits the tick's remaining RAM budget — checked even
    /// for an `over` (overflow-slot) candidate, since a star cannot buy
    /// memory — and otherwise, unless `over`, when its repo is at its #9090
    /// cap ([`Self::defer`]). A RAM deferral counts as `deferred_capacity`
    /// with a `ram:` detail naming the repo and its charge. Work-conserving:
    /// the next candidate, in a lighter repo, may still fit.
    pub fn defer_admission(
        &self,
        cand: &PriorityCandidate,
        over: bool,
        report: &mut TickReport,
    ) -> bool {
        if let Some(ram) = self.ram.as_ref().filter(|r| !r.fits(cand.workspace_idx)) {
            let detail = ram.deferral_detail(cand.workspace_idx);
            log::debug!("work_finder: deferring issue #{} — {detail}", cand.number);
            report.deferred_capacity += 1;
            ready_queue::resolve(&mut report.queue, cand, Qd::DeferredCapacity, Some(detail));
            return true;
        }
        // #11191: the same for the repo's disk charge; a star cannot buy disk.
        if let Some(disk) = self.disk.as_ref().filter(|d| !d.fits(cand.workspace_idx)) {
            let detail = disk.deferral_detail(cand.workspace_idx);
            log::debug!("work_finder: deferring issue #{} — {detail}", cand.number);
            report.deferred_capacity += 1;
            ready_queue::resolve(&mut report.queue, cand, Qd::DeferredCapacity, Some(detail));
            return true;
        }
        if !over && self.defer(cand, report) {
            return true;
        }
        let shared = self.overlap.overlapping(cand);
        if shared.is_empty() {
            return false;
        }
        // #9781: scheduling signal only — no label, hold, or stacking edge.
        let detail = ready_queue::short_detail(&format!("file overlap: {}", shared.join(", ")));
        ready_queue::resolve(&mut report.queue, cand, Qd::DeferredFileOverlap, Some(detail));
        true
    }

    /// Record a listed item's `## Affected Files` surface (#9781); `occupying`
    /// marks an already-in-flight item. Reads only the listing's own body.
    pub fn note_surface(&mut self, idx: usize, item: &super::WorkItem, occupying: bool) {
        self.overlap
            .note(idx, item.number, item.body.as_deref(), occupying);
    }

    /// [`Self::admit`] for a whole candidate: also occupies its surface.
    pub fn admit_candidate(&mut self, cand: &PriorityCandidate) {
        self.overlap.occupy(cand);
        self.admit(cand.workspace_idx);
    }

    /// The cap and the per-workspace occupancy as they stand now — at the
    /// top of the tick when [`shape_queue`] records it (Issue #9288).
    #[must_use]
    pub fn snapshot(&self) -> RepoCapSnapshot {
        RepoCapSnapshot {
            cap: self.cap,
            occupancy: self.occupancy.clone(),
        }
    }

    /// Count one successful dispatch against workspace `idx`'s cap — the
    /// per-repo twin of pass 2's `occupancy += 1`.
    pub fn admit(&mut self, idx: usize) {
        if let Some(slot) = self.occupancy.get_mut(idx) {
            *slot += 1;
        }
        if let Some(ram) = self.ram.as_mut() {
            ram.debit(idx);
        }
        if let Some(disk) = self.disk.as_mut() {
            disk.debit(idx);
        }
    }
}

/// A [`RepoCap`] as the tick shaped with it (Issue #9288): the cap, and each
/// workspace's live-sweep count at the top of the tick (`> 0` ⇒ a hot track).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoCapSnapshot {
    pub cap: Option<usize>,
    pub occupancy: Vec<usize>,
}

/// Shape the globally-sorted candidate list for pass 2: the #6243 repo-sharding
/// slice partition, then (when a per-repo cap is configured) the #9090 track
/// affinity partition.
///
/// **Lane candidates are exempt** (#9244): a starred or red-main-fix candidate
/// stays at the head of the list, in its [`candidate_cmp`](super::candidate_cmp)
/// order, and both partitions apply only to the ordinary tail behind it.
/// Affinity would otherwise float every hot-repo candidate ahead of a starred
/// issue in a cold repo, and the slice would defer a starred issue whose repo
/// another host prefers; either breaks "starred first, fleet-wide". A lane
/// candidate's repo still counts toward the slice's "is my slice empty?"
/// question, so the #6243 fallback behaves as before.
///
/// Both partitions are **stable**, so `candidate_cmp` still decides order
/// within each group and the comparator itself is untouched. With
/// `preferred_slice: None` and a disabled `cap` this returns `candidates`
/// unchanged.
///
/// Records the shaped order and both shaping inputs on `report` (Issue
/// #9288), so the published dispatch plan reads the order pass 2 actually
/// iterates instead of re-deriving it.
#[must_use]
pub fn shape_queue(
    candidates: Vec<PriorityCandidate>,
    preferred_slice: Option<&[bool]>,
    cap: &RepoCap,
    report: &mut TickReport,
) -> Vec<PriorityCandidate> {
    let shaped = shape(candidates, preferred_slice, cap, report);
    report.plan_order = shaped.iter().map(|c| (c.workspace_idx, c.number)).collect();
    report.in_slice = preferred_slice.map(<[bool]>::to_vec);
    report.repo_cap = Some(cap.snapshot());
    shaped
}

fn shape(
    candidates: Vec<PriorityCandidate>,
    preferred_slice: Option<&[bool]>,
    cap: &RepoCap,
    report: &mut TickReport,
) -> Vec<PriorityCandidate> {
    // `candidates` is sorted by `candidate_cmp`, whose first keys are the
    // lanes, so this partition keeps the head exactly as sorted.
    let (lanes, rest): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|c| c.operator_priority || c.main_red_fix);
    let in_slice = |c: &PriorityCandidate| {
        preferred_slice.is_none_or(|s| s.get(c.workspace_idx).copied().unwrap_or(true))
    };
    let lane_in_slice = lanes.iter().any(in_slice);
    let sliced = apply_slice(rest, preferred_slice, lane_in_slice, report);
    if !cap.enabled() {
        return lanes.into_iter().chain(sliced).collect();
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
    lanes.into_iter().chain(hot).chain(cold).collect()
}

/// The #6243 repo-sharding slice partition, moved here verbatim from
/// `tick_multi_with_sharding` (#9090) so its sibling affinity pass sits next to
/// it and `work_finder.rs` — frozen by the file-size ratchet — does not grow.
///
/// `None` is a no-op. Otherwise the already-sorted list splits into in-slice /
/// out-of-slice preserving each partition's relative order; out-of-slice
/// candidates dispatch THIS TICK only when the slice was completely empty at
/// the top of the tick (work-conservation, #6243's AC). `lane_in_slice` says
/// an exempt lane candidate (see [`shape_queue`]) sits in the slice, so the
/// slice is not empty.
fn apply_slice(
    candidates: Vec<PriorityCandidate>,
    preferred_slice: Option<&[bool]>,
    lane_in_slice: bool,
    report: &mut TickReport,
) -> Vec<PriorityCandidate> {
    let Some(slice) = preferred_slice else {
        return candidates;
    };
    let (in_slice, out_of_slice): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|c| slice.get(c.workspace_idx).copied().unwrap_or(true));
    if in_slice.is_empty() && !lane_in_slice {
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
        max_concurrent.into(),
        halted,
        None,
        max_admissions_per_tick,
        saturation_held,
        preferred_slice,
        None,
        &[],
    )
}

#[cfg(test)]
#[path = "repo_cap_tests.rs"]
mod tests;
