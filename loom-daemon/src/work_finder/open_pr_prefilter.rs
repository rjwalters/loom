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
//! a verified open linked PR ([`WorkDispatcher::open_pr_answer`]; in
//! production one read of the ETag-cached open-PR listing, see
//! [`answer_in`]). A candidate in that answer is resolved exactly as the
//! in-`dispatch()` refusal resolves it — `QueueDisposition::OpenPr`, detail
//! `open PR #X`, counted in `skipped_pr_open` — without `dispatch_with` being
//! called. Every other candidate goes to `dispatch_with` as before, unless
//! the landed pre-filter (#11853, [`super::landed_prefilter`], asked once per
//! workspace per tick about the candidates without an open PR) finds its
//! closing PR already merged: that one is resolved `AlreadyLanded`.
//!
//! # What it cannot do
//!
//! - **Admit.** It only ever removes a candidate. A candidate it does not
//!   remove goes through `dispatch()`'s own guard chain, unchanged and still
//!   authoritative; "not in the answer" is never cached as a permit.
//! - **Remove on a guess.** Only a verified `Open` verdict from a listing that
//!   was read removes a candidate as `OpenPr`. An unresolvable repo, guards
//!   switched off, a poisoned lock: nothing was asked
//!   ([`OpenPrAnswer::NotAsked`]) and every candidate proceeds to
//!   `dispatch()` as on the pre-#8865 path. An unreadable listing is not
//!   that: see "Failing closed" below.
//! - **Reorder.** It runs at the dispatch point, after the #11103 workspace
//!   draw and every admission gate, so the draw sees the same candidates and
//!   makes the same rolls as without it. The candidates that survive are
//!   attempted in the order they would have been attempted anyway; the
//!   removed ones are the ones `dispatch()` would have refused in place.
//! - **Outlive the tick.** The answer is held for one tick. A PR that closes
//!   unmerged frees its issue on the next tick whose listing read shows it.
//!   (The listing under it may be up to
//!   [`LISTING_REUSE_FOR`](crate::worktree_ops::linked_pr_listing::LISTING_REUSE_FOR)
//!   old when the read failed, #12025.)
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
//! # Failing closed (#12025)
//!
//! When the listing could not be read (and no good listing of the last few
//! minutes can stand in), the old path sent every candidate to `dispatch()`,
//! whose 2.6 guard then probed each one on its own: the closes-graph and the
//! timeline walk per candidate, since leg 0 is off for the root after the
//! failed read. Its refusals take no slot, so one bad tick walked the whole
//! queue: about 300 probes in 90 s on every worker at once. Now a candidate
//! the listing did not verify is not dispatched that tick when
//!
//! - the workspace's open-PR listing is unreadable
//!   ([`OpenPrAnswer::Unreadable`]), or
//! - its ready-issue listing came back incomplete this tick
//!   ([`TickReport::listing_incomplete`]) and the open-PR listing gave this
//!   candidate no verdict.
//!
//! It is resolved `DispatchError`, detail [`FAIL_CLOSED_DETAIL`], and counted
//! as one `forge_listing` error ([`DispatchErrorKind::ForgeListing`]): no
//! forge call, no dispatch attempt, no backoff armed. It is listed again next
//! tick. A candidate the listing verified `NoneOpen` goes on to `dispatch()`
//! as before; one it verified `Open` is skipped `OpenPr` as before.
//!
//! # The #6788 memo
//!
//! The guard's verified-open-PR memo used to be written by the 2.6 probe each
//! time a PR-blocked candidate was refused. Those candidates no longer reach
//! 2.6, so [`answer_in`] folds what the listing verified into the memo
//! instead: `Open(pr)` records or refreshes the entry, a verified "no open
//! PR" removes it, and an unreadable or reused (#12025) listing changes
//! nothing. That keeps the guard's fallback alive for the tick the listing cannot be read: a fresh
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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use super::tick_report::record_dispatch_outcome;
use super::{DispatchErrorKind, PriorityCandidate, TickReport, WorkDispatcher};
use crate::sweep_registry::SweepRegistry;
use crate::types::QueueDisposition as Qd;
use crate::worktree_ops::gh::OpenPrProbe;

/// The conditional-read caller name of the pre-filter's listing read.
const CALLER: &str = "work_finder.open_pr_prefilter";

/// The queue-row detail of a candidate the pre-filter failed closed on
/// (#12025). Fixed text: it names no repo, path or forge output.
pub const FAIL_CLOSED_DETAIL: &str =
    "open-PR listing unreadable or partial: not verified, not dispatched this tick (#12025)";

/// What a workspace's open-PR listing answered about its candidates
/// ([`WorkDispatcher::open_pr_answer`], #12025).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenPrAnswer {
    /// No listing was consulted (guards off, no resolvable repo, a dispatcher
    /// without one): these are the verified-open candidates, if any, and
    /// every other candidate goes to `dispatch()` as before #12025.
    NotAsked(HashMap<u32, u32>),
    /// A listing answered: `blocked` maps the candidates with a verified
    /// open linked PR to it, `none_open` holds those verified to have none.
    /// A candidate in neither has no verdict.
    Listed {
        blocked: HashMap<u32, u32>,
        none_open: HashSet<u32>,
    },
    /// The listing could not be read: no candidate is verified.
    Unreadable,
}

impl OpenPrAnswer {
    /// The verified-open candidates, each mapped to its PR.
    #[must_use]
    pub fn into_blocked(self) -> HashMap<u32, u32> {
        match self {
            Self::NotAsked(blocked) | Self::Listed { blocked, .. } => blocked,
            Self::Unreadable => HashMap::new(),
        }
    }

    fn open_pr(&self, issue: u32) -> Option<u32> {
        match self {
            Self::NotAsked(blocked) | Self::Listed { blocked, .. } => blocked.get(&issue).copied(),
            Self::Unreadable => None,
        }
    }

    /// Whether `issue` must not be dispatched this tick: nothing verified it
    /// and the listings under it are not whole (see the module docs).
    fn fails_closed(&self, issue: u32, candidates_incomplete: bool) -> bool {
        match self {
            Self::NotAsked(_) => false,
            Self::Unreadable => true,
            Self::Listed { blocked, none_open } => {
                candidates_incomplete
                    && !blocked.contains_key(&issue)
                    && !none_open.contains(&issue)
            }
        }
    }
}

/// One tick's pre-filter state. See the module docs.
#[derive(Debug, Default)]
pub struct OpenPrPrefilter {
    /// Every candidate of the tick, per workspace index.
    candidates: HashMap<usize, Vec<u32>>,
    /// Per workspace already asked this tick: what its listing answered.
    blocked: HashMap<usize, OpenPrAnswer>,
    /// Workspaces whose fail-closed hold was already logged this tick.
    failed_closed: HashSet<usize>,
    /// Per workspace already asked this tick: open issue to its merged
    /// closing PR (#11853, [`super::landed_prefilter`]).
    landed: HashMap<usize, HashMap<u32, u32>>,
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
            failed_closed: HashSet::new(),
            landed: HashMap::new(),
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
        if self.fails_closed(cand, report) {
            report.note_error(DispatchErrorKind::ForgeListing);
            return (Qd::DispatchError, Some(FAIL_CLOSED_DETAIL.to_string()));
        }
        if let Some(pr) = self.landed_pr(dispatcher, cand) {
            report.skipped_already_landed += 1;
            return (Qd::AlreadyLanded, Some(format!("merged closing PR #{pr}")));
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
            self.blocked.insert(idx, dispatcher.open_pr_answer(asked));
        }
        self.blocked.get(&idx)?.open_pr(cand.number)
    }

    /// Whether `cand` is held this tick because nothing verified it and its
    /// workspace's listings are not whole (#12025). Called only after
    /// [`Self::open_pr`] answered. Logs the hold once per workspace per tick.
    fn fails_closed(&mut self, cand: &PriorityCandidate, report: &TickReport) -> bool {
        let idx = cand.workspace_idx;
        let incomplete = report.listing_incomplete.contains(&idx);
        let Some(answer) = self.blocked.get(&idx) else {
            return false;
        };
        if !answer.fails_closed(cand.number, incomplete) {
            return false;
        }
        if self.failed_closed.insert(idx) {
            let why = if matches!(answer, OpenPrAnswer::Unreadable) {
                "its open-PR listing could not be read"
            } else {
                "its ready-issue listing is incomplete and the open-PR listing gave no verdict"
            };
            log::warn!(
                "work_finder: workspace #{idx}: {why}; its unverified candidates are not \
                 dispatched this tick, so no per-issue open-PR probe runs for them (fail \
                 closed, #12025)"
            );
        }
        true
    }

    /// `cand`'s merged closing PR while it stays open (#11853), asking its
    /// workspace's dispatcher once per tick about every candidate without an
    /// open linked PR. Called only after [`Self::open_pr`] answered.
    fn landed_pr<D: WorkDispatcher>(
        &mut self,
        dispatcher: &D,
        cand: &PriorityCandidate,
    ) -> Option<u32> {
        let idx = cand.workspace_idx;
        if !self.landed.contains_key(&idx) {
            let blocked = self.blocked.get(&idx);
            let is_blocked = |n: &u32| blocked.is_some_and(|b| b.open_pr(*n).is_some());
            let all = self.candidates.get(&idx).map_or(&[][..], Vec::as_slice);
            let (open_pr, asked): (Vec<u32>, Vec<u32>) = all.iter().partition(|n| is_blocked(n));
            let answer = dispatcher.landed_closing_prs(&asked, &open_pr);
            self.landed.insert(idx, answer);
        }
        self.landed.get(&idx)?.get(&cand.number).copied()
    }
}

/// [`WorkDispatcher::open_pr_blocked`] for the production dispatcher: the
/// verified-open part of [`answer_in`]. Empty whenever that cannot be
/// verified.
#[must_use]
pub fn blocked_in(registry: &Arc<Mutex<SweepRegistry>>, candidates: &[u32]) -> HashMap<u32, u32> {
    answer_in(registry, candidates).into_blocked()
}

/// [`WorkDispatcher::open_pr_answer`] for the production dispatcher: the
/// verdict for each of `candidates` from one read of the workspace's open-PR
/// listing, or from the last good read of the past few minutes when this one
/// fails (#12025). [`OpenPrAnswer::Unreadable`] when neither is there;
/// [`OpenPrAnswer::NotAsked`] when the guards are off or the repo cannot be
/// resolved, as `dispatch()`'s own 2.5/2.6 steps would not probe either.
///
/// The registry lock is taken twice, briefly, and never across the listing
/// read: once to copy the three inputs of the read
/// (`SweepRegistry::open_pr_listing_target`), and once after a fresh read to
/// fold the verified verdicts into the #6788 memo
/// (`SweepRegistry::fold_listing_verdicts`). Resolving the repo in the first
/// can itself call `gh` (see that method); the listing read cannot stall a
/// status IPC call the way the in-`dispatch()` guard chain can (#9573).
#[must_use]
pub fn answer_in(registry: &Arc<Mutex<SweepRegistry>>, candidates: &[u32]) -> OpenPrAnswer {
    if candidates.is_empty() {
        return OpenPrAnswer::NotAsked(HashMap::new());
    }
    let target = match registry.lock() {
        Ok(reg) => reg.open_pr_listing_target(),
        Err(poisoned) => {
            log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
            None
        }
    };
    let Some((gh, root, owner_repo)) = target else {
        return OpenPrAnswer::NotAsked(HashMap::new());
    };
    let timeout = Some(crate::sweep_registry::reaper::reap_gh_timeout());
    let asked = (owner_repo.as_str(), candidates);
    let read_started = chrono::Utc::now();
    let listed =
        crate::worktree_ops::linked_pr_listing::probe_many(CALLER, &gh, &root, asked, timeout);
    let Some(listed) = listed else {
        log::debug!(
            "work_finder: open-PR pre-filter for {owner_repo} has no listing this tick and none \
             recent; its {} candidate(s) fail closed, and the open-PR memo is left as it was \
             (#8865, #12025)",
            candidates.len()
        );
        return OpenPrAnswer::Unreadable;
    };
    let verdicts = listed.verdicts;
    if let Some(age) = listed.reused {
        log::debug!(
            "work_finder: open-PR pre-filter for {owner_repo}: listing read failed; using the \
             last good listing, {}s old (#12025)",
            age.as_secs()
        );
    } else {
        // Keep the #6788 memo fed: these candidates no longer reach the 2.6
        // probe that used to write it. A second short lock, after the read,
        // no forge call inside it. Never from a reused listing: its verdicts
        // are not as of `read_started`.
        match registry.lock() {
            Ok(reg) => reg.fold_listing_verdicts(read_started, &verdicts),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
            }
        }
    }
    let mut blocked = HashMap::new();
    let mut none_open = HashSet::new();
    for (issue, verdict) in &verdicts {
        match verdict {
            OpenPrProbe::Open(pr) => {
                blocked.insert(*issue, *pr);
            }
            OpenPrProbe::NoneOpen => {
                none_open.insert(*issue);
            }
            OpenPrProbe::ProbeFailed => {}
        }
    }
    log::debug!(
        "work_finder: open-PR pre-filter for {owner_repo}: {} of {} candidate(s) have an open \
         linked PR, from one listing read (#8865)",
        blocked.len(),
        candidates.len()
    );
    log_changes(&root, &blocked);
    OpenPrAnswer::Listed { blocked, none_open }
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
