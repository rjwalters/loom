//! The batched open-PR read (Issue #8865): which of a workspace's issues have
//! a verified open linked PR, from one read of its open-PR listing.
//!
//! # Where it is used
//!
//! It was built for the multi-workspace tick's dispatch point, where a
//! PR-blocked candidate used to cost a whole serialized dispatch attempt
//! before the #4123 guard refused it. That tick was retired with the
//! Builder-only finder path (#11911). Since then the read serves
//! [`WorkDispatcher::open_pr_blocked`](super::WorkDispatcher::open_pr_blocked),
//! whose production implementation ([`blocked_in`]) the red-main auto-filed
//! fix (#11766, `RegistryDispatcher::autofile_red_main` →
//! `main_red_autofile::tick`) asks which of its issues already have an open
//! PR. The step path reads its own listing in `step_dispatch::gather`.
//!
//! # What it answers
//!
//! [`answer_in`] reads the workspace's ETag-cached open-PR listing once for
//! every asked issue and classifies each with the open-PR guard's own
//! classifier. [`blocked_in`] keeps the verified-open part: issue to its open
//! linked PR.
//!
//! - **Only verified `Open` verdicts.** An unresolvable repo, guards switched
//!   off or a poisoned lock asks nothing
//!   ([`OpenPrAnswer::NotAsked`]); an unreadable listing is
//!   [`OpenPrAnswer::Unreadable`]. Both leave [`blocked_in`] empty, and "not
//!   in the answer" is never a permit.
//! - **Listing reuse (#12025).** When a read fails, the last good listing of
//!   the same root and repo answers instead for up to
//!   [`LISTING_REUSE_FOR`](crate::worktree_ops::linked_pr_listing::LISTING_REUSE_FOR),
//!   so one failed page does not blank the answer. Past that it is
//!   `Unreadable`.
//! - **One read per call.** However many issues are asked, and no
//!   per-issue read.
//!
//! # The #6788 memo
//!
//! A fresh read folds its verdicts into the open-PR guard's
//! verified-open-PR memo: `Open(pr)` records or refreshes the entry, a
//! verified "no open PR" removes it. An unreadable or reused listing changes
//! nothing, since its verdicts are not as of this read. That keeps the
//! guard's fallback fed for a tick the listing cannot be read: a fresh entry
//! refuses at step 2.5, and an entry of any age gives the known-PR recheck
//! (`GET pulls/{pr}`) something to recheck.
//!
//! # Logging
//!
//! [`log_changes`] logs a skip line when an issue becomes blocked or its PR
//! changes, not on every read while it stays blocked.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::sweep_registry::SweepRegistry;
use crate::worktree_ops::gh::OpenPrProbe;

/// The conditional-read caller name of the pre-filter's listing read.
const CALLER: &str = "work_finder.open_pr_prefilter";

/// What a workspace's open-PR listing answered about its candidates
/// ([`answer_in`], #12025).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenPrAnswer {
    /// No listing was consulted (guards off, no resolvable repo, a dispatcher
    /// without one): these are the verified-open candidates, if any, and
    /// every other candidate goes to `dispatch()` as before #12025.
    NotAsked(HashMap<u32, u32>),
    /// A listing answered: `blocked` maps the candidates with a verified
    /// open linked PR to it.
    Listed { blocked: HashMap<u32, u32> },
    /// The listing could not be read: no candidate is verified.
    Unreadable,
}

impl OpenPrAnswer {
    /// The verified-open candidates, each mapped to its PR.
    #[must_use]
    pub fn into_blocked(self) -> HashMap<u32, u32> {
        match self {
            Self::NotAsked(blocked) | Self::Listed { blocked } => blocked,
            Self::Unreadable => HashMap::new(),
        }
    }
}

/// [`WorkDispatcher::open_pr_blocked`](super::WorkDispatcher::open_pr_blocked)
/// for the production dispatcher: the verified-open part of [`answer_in`].
/// Empty whenever that cannot be verified.
#[must_use]
pub fn blocked_in(registry: &Arc<Mutex<SweepRegistry>>, candidates: &[u32]) -> HashMap<u32, u32> {
    answer_in(registry, candidates).into_blocked()
}

/// The verdict for each of `candidates` from one read of the workspace's open-PR
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
    for (issue, verdict) in &verdicts {
        if let OpenPrProbe::Open(pr) = verdict {
            blocked.insert(*issue, *pr);
        }
    }
    log::debug!(
        "work_finder: open-PR pre-filter for {owner_repo}: {} of {} candidate(s) have an open \
         linked PR, from one listing read (#8865)",
        blocked.len(),
        candidates.len()
    );
    log_changes(&root, &blocked);
    OpenPrAnswer::Listed { blocked }
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
