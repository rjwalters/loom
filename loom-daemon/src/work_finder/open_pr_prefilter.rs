//! The work finder's batched open-PR pre-filter (Issue #8865).
//!
//! # The problem
//!
//! The #4123 open-PR guard lives inside `dispatch()` (steps 2.5/2.6 of
//! `begin_prepared_issue_dispatch`), deliberately: one check covers every
//! dispatch route. For the finder that placement made every PR-blocked
//! candidate cost a whole serialized dispatch attempt — runtime and model
//! resolution, account selection, then under the registry mutex a
//! closed-issue read and a walk of the repo's open-PR listing — only to be
//! refused. The listing is the same for every candidate of a workspace, and
//! it was walked once per candidate. Each refusal also armed the issue's
//! #4485 dispatch backoff, as a "failed dispatch".
//!
//! # What this does
//!
//! Pass 2 of the multi-workspace tick calls [`OpenPrPrefilter::dispatch`]
//! where it used to call `dispatch_with`. The first time a candidate of a
//! workspace reaches that point in a tick, the pre-filter asks the
//! workspace's dispatcher ONCE for every candidate of that workspace that has
//! a verified open linked PR ([`WorkDispatcher::open_pr_blocked`]; in
//! production one read of the ETag-cached open-PR listing, see
//! [`blocked_in`]). A candidate in that answer is resolved exactly as the
//! in-`dispatch()` refusal resolves it — `QueueDisposition::OpenPr`, detail
//! `open PR #X`, counted in `skipped_pr_open` — without `dispatch_with` being
//! called. Every other candidate goes to `dispatch_with` as before.
//!
//! # What it cannot do
//!
//! - **Admit.** It only ever removes a candidate. A candidate it does not
//!   remove goes through `dispatch()`'s own guard chain, unchanged and still
//!   authoritative; "not in the answer" is never cached as a permit.
//! - **Remove on a guess.** Only a verified `Open` verdict from a listing that
//!   was read removes a candidate. An unreadable listing, an unresolvable
//!   repo, guards switched off, a poisoned lock: the answer is empty and every
//!   candidate proceeds to `dispatch()` as on the pre-#8865 path.
//! - **Reorder.** It runs at the dispatch point, after the #11103 workspace
//!   draw and every admission gate, so the draw sees the same candidates and
//!   makes the same rolls as without it. The candidates that survive are
//!   attempted in the order they would have been attempted anyway; the
//!   removed ones are the ones `dispatch()` would have refused in place.
//! - **Outlive the tick.** The answer is held for one tick. A PR that closes
//!   unmerged frees its issue on the next tick whose listing read shows it.
//! - **Arm a backoff.** No dispatch was attempted, so nothing is recorded
//!   against the issue: no #4485 window, no peer claim.
//!
//! # The one change in precedence
//!
//! `dispatch()` runs its `prepare` phase and the 2.4 workspace-commands guard
//! before the open-PR guard, so a PR-blocked candidate that would also have
//! failed one of those (an empty token pool, a rejected runtime) used to be
//! reported under that failure. It is now reported `OpenPr`, which is true of
//! it either way, and it no longer arms the failure's backoff. Candidates
//! without an open PR still meet those checks unchanged.
//!
//! # The #6788 memo
//!
//! The guard's verified-open-PR memo used to be written by the 2.6 probe each
//! time a PR-blocked candidate was refused. Those candidates no longer reach
//! 2.6, so [`blocked_in`] folds what the listing verified into the memo
//! instead: `Open(pr)` records or refreshes the entry, a verified "no open
//! PR" removes it, and an unreadable listing changes nothing. That keeps the
//! guard's fallback alive for the tick the listing cannot be read: a fresh
//! entry refuses at 2.5, and an entry of any age gives the known-PR recheck
//! (`GET pulls/{pr}`) something to recheck when GraphQL and the timeline fail
//! too. Removal is what frees an issue on the tick its PR is gone, rather
//! than up to 900 s later.
//!
//! # Cost
//!
//! No forge read at all on a tick where no candidate reaches the dispatch
//! point (idle, halted, saturated, at capacity): the read is lazy. Otherwise
//! one listing read per workspace per tick, however many of its candidates
//! are blocked, and no per-candidate issue read for the removed ones.
//!
//! # Telemetry
//!
//! A pre-filtered candidate keeps its `OpenPr` ready-queue row on every tick
//! it reaches the dispatch point, so the #9674 lockout metrics (which key on
//! `OpenPr` rows) and the `pr-open-skip` tick counter keep reporting it. It
//! gets no `loom.dispatch.admission` span: that span is one per `dispatch()`
//! attempt, and none was made. [`TickReport::pr_open_prefiltered`] counts
//! these skips apart from the refusals `dispatch()` still makes itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use super::tick_report::record_dispatch_outcome;
use super::{PriorityCandidate, TickReport, WorkDispatcher};
use crate::sweep_registry::SweepRegistry;
use crate::types::QueueDisposition as Qd;
use crate::worktree_ops::gh::OpenPrProbe;

/// The conditional-read caller name of the pre-filter's listing read.
const CALLER: &str = "work_finder.open_pr_prefilter";

/// One tick's pre-filter state. See the module docs.
#[derive(Debug, Default)]
pub struct OpenPrPrefilter {
    /// Every candidate of the tick, per workspace index.
    candidates: HashMap<usize, Vec<u32>>,
    /// Per workspace already asked this tick: issue to its open linked PR.
    blocked: HashMap<usize, HashMap<u32, u32>>,
}

impl OpenPrPrefilter {
    /// The pre-filter for a tick whose pass 2 will walk `candidates`.
    #[must_use]
    pub fn new(candidates: &[PriorityCandidate]) -> Self {
        let mut by_workspace: HashMap<usize, Vec<u32>> = HashMap::new();
        for cand in candidates {
            by_workspace
                .entry(cand.workspace_idx)
                .or_default()
                .push(cand.number);
        }
        Self {
            candidates: by_workspace,
            blocked: HashMap::new(),
        }
    }

    /// Dispatch `cand` through its workspace's `dispatcher` unless the
    /// pre-filter verified an open linked PR for it, and count the outcome on
    /// `report`. Returns the ready-queue disposition and detail, like
    /// [`record_dispatch_outcome`]; only [`Qd::Dispatched`] consumed a slot.
    pub fn dispatch<D: WorkDispatcher>(
        &mut self,
        dispatcher: &mut D,
        cand: &PriorityCandidate,
        overflow: bool,
        report: &mut TickReport,
    ) -> (Qd, Option<String>) {
        if let Some(pr) = self.open_pr(dispatcher, cand) {
            report.skipped_pr_open += 1;
            report.pr_open_prefiltered += 1;
            return (Qd::OpenPr, Some(format!("open PR #{pr}")));
        }
        let started = chrono::Utc::now();
        let outcome = dispatcher.dispatch_with(cand.number, overflow);
        record_dispatch_outcome(report, cand.number, cand.workspace_idx, started, &outcome)
    }

    /// `cand`'s verified open linked PR, asking its workspace's dispatcher on
    /// the workspace's first candidate of the tick and never again.
    fn open_pr<D: WorkDispatcher>(
        &mut self,
        dispatcher: &D,
        cand: &PriorityCandidate,
    ) -> Option<u32> {
        let idx = cand.workspace_idx;
        if !self.blocked.contains_key(&idx) {
            let asked = self.candidates.get(&idx).map_or(&[][..], Vec::as_slice);
            self.blocked.insert(idx, dispatcher.open_pr_blocked(asked));
        }
        self.blocked.get(&idx)?.get(&cand.number).copied()
    }
}

/// [`WorkDispatcher::open_pr_blocked`] for the production dispatcher: the
/// issues in `candidates` with a verified open linked PR, from one read of the
/// workspace's open-PR listing. Empty whenever that cannot be verified.
///
/// The registry lock is taken twice, briefly, and never across the listing
/// read: once to copy the three inputs of the read
/// (`SweepRegistry::open_pr_listing_target`), and once after it to fold the
/// verified verdicts into the #6788 memo
/// (`SweepRegistry::fold_listing_verdicts`). Resolving the repo in the first
/// can itself call `gh` (see that method); the listing read cannot stall a
/// status IPC call the way the in-`dispatch()` guard chain can (#9573).
#[must_use]
pub fn blocked_in(registry: &Arc<Mutex<SweepRegistry>>, candidates: &[u32]) -> HashMap<u32, u32> {
    if candidates.is_empty() {
        return HashMap::new();
    }
    let target = match registry.lock() {
        Ok(reg) => reg.open_pr_listing_target(),
        Err(poisoned) => {
            log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
            None
        }
    };
    let Some((gh, root, owner_repo)) = target else {
        return HashMap::new();
    };
    let timeout = Some(crate::sweep_registry::reaper::reap_gh_timeout());
    let asked = (owner_repo.as_str(), candidates);
    let read_started = chrono::Utc::now();
    let listed =
        crate::worktree_ops::linked_pr_listing::probe_many(CALLER, &gh, &root, asked, timeout);
    let Some(verdicts) = listed else {
        log::debug!(
            "work_finder: open-PR pre-filter for {owner_repo} has no listing this tick; all {} \
             candidate(s) go to dispatch() and its own guard, and the open-PR memo is left as \
             it was (#8865)",
            candidates.len()
        );
        return HashMap::new();
    };
    // Keep the #6788 memo fed: these candidates no longer reach the 2.6 probe
    // that used to write it. A second short lock, after the read, no forge
    // call inside it.
    match registry.lock() {
        Ok(reg) => reg.fold_listing_verdicts(read_started, &verdicts),
        Err(poisoned) => {
            log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
        }
    }
    let blocked: HashMap<u32, u32> = verdicts
        .iter()
        .filter_map(|(issue, verdict)| match verdict {
            OpenPrProbe::Open(pr) => Some((*issue, *pr)),
            OpenPrProbe::NoneOpen | OpenPrProbe::ProbeFailed => None,
        })
        .collect();
    log::debug!(
        "work_finder: open-PR pre-filter for {owner_repo}: {} of {} candidate(s) have an open \
         linked PR, from one listing read (#8865)",
        blocked.len(),
        candidates.len()
    );
    log_changes(&root, &blocked);
    blocked
}

/// What the pre-filter last reported blocked, per workspace root.
fn last_reported() -> &'static Mutex<HashMap<PathBuf, HashMap<u32, u32>>> {
    static LAST: OnceLock<Mutex<HashMap<PathBuf, HashMap<u32, u32>>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Log the skip line for each issue in `blocked` that was not reported
/// blocked by the same PR on `root`'s previous read. A standing block is one
/// line, not one per tick; the tick line's `pr-open-skip` count carries the
/// steady state. State is replaced with `blocked`, so memory is bounded by
/// one tick's blocked candidates per workspace.
pub(super) fn log_changes(root: &Path, blocked: &HashMap<u32, u32>) {
    let mut last = last_reported().lock().unwrap_or_else(|p| p.into_inner());
    let before = last.get(root);
    let mut fresh: Vec<(u32, u32)> = blocked
        .iter()
        .filter(|(issue, pr)| before.and_then(|b| b.get(*issue)) != Some(*pr))
        .map(|(issue, pr)| (*issue, *pr))
        .collect();
    fresh.sort_unstable();
    for (issue, pr) in fresh {
        log::info!(
            "work_finder: skipping issue #{issue} — it already has an open linked PR #{pr} \
             (#4123 open-PR guard; pre-filtered from the open-PR listing with no dispatch \
             attempt, logged once while this holds, #8865)"
        );
    }
    last.insert(root.to_path_buf(), blocked.clone());
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "open_pr_prefilter_tests.rs"]
mod tests;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "open_pr_prefilter_forge_tests.rs"]
mod forge_tests;
