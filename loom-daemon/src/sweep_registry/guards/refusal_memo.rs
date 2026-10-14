//! The dispatch path's use of the verified-open-PR memo, and the open-PR
//! refusal ledger (W9 of the forge API reduction plan).
//!
//! # The memo serves refusals only, never a resume
//!
//! The #6788 memo ([`SweepRegistry::fresh_open_pr_memo`]) is in memory, fresh
//! for [`OPEN_PR_MEMO_FRESH`] (900 s) from its last verified answer: past that, the next
//! attempt runs the live probe, whose leg 0 (#10514) reads the ETag'd open-PR
//! listing (a free `304` on an unchanged repo). For an ordinary dispatch a
//! memo answer can only **refuse**: the 2.5 short circuit returns
//! `OpenPrDispatchError` without spending a call.
//!
//! A crash resume (#4256, `resume_bypass_pr = Some(pr)`) is the one place an
//! `Open(pr)` answer **permits** work: the reaper resumes because the probe
//! named a PR, and 2.6 then lets the dispatch through for that PR. So every
//! resume decision skips the fresh-memo short circuit and probes the forge:
//!
//! - the reaper's eligibility probe calls [`SweepRegistry::live_open_pr_probe`];
//! - [`SweepRegistry::dispatch_open_pr_memo`] never short-circuits a resume;
//! - [`SweepRegistry::dispatch_open_pr_probe`] runs the live probe for one.
//!
//! Caveat: when every transport fails, the live probe's #6788 known-PR
//! backstop takes the PR number from a memo entry of **any** age and checks
//! only that PR's openness live. On that path the link itself is not
//! re-verified.
//!
//! The other memo readers (the reaper's #4366 no-progress exemption, the
//! PR-less retry tally and its `loom:blocked` hold veto) keep today's
//! fresh-only memo: each answer there is at most 900 s old, as before.
//!
//! # Who writes the memo
//!
//! The live probe ([`SweepRegistry::live_open_pr_probe`]), the reaper's
//! post-exit seed (#8355), and the work finder's batched pre-filter
//! ([`SweepRegistry::fold_listing_verdicts`], #8865). The third exists because
//! a pre-filtered candidate never reaches the 2.6 probe: its verified listing
//! verdict is folded in instead, so the known-PR backstop still has a PR to
//! recheck for it. Each write comes from a new verified answer; nothing
//! extends an entry without one.
//!
//! # The ledger: distinct versus repeated refusals
//!
//! Every open-PR refusal bumps one of two `loom.forge.facade.events`
//! counters: [`REFUSED_MEMO`] (the 2.5 short circuit, zero calls) or
//! [`REFUSED_PROBED`] (the 2.6 probe, which for an ordinary dispatch can
//! itself be answered from the memo when an entry became fresh after 2.5). A refusal of an
//! `(issue, PR)` pair this workspace has not refused yet in the current UTC
//! hour also bumps [`REFUSED_DISTINCT`]. Per hour, `distinct / (memo +
//! probed)` is the share of refusals that were new, which is what the plan's
//! open question 9 asks.

use super::*;
use std::collections::HashSet;

/// An open-PR refusal served by the fresh memo (no forge call).
pub(crate) const REFUSED_MEMO: &str = "guard.open_pr.refused_memo";
/// An open-PR refusal at the 2.6 probe. For an ordinary dispatch that probe
/// is memo-aware, so a memo entry that became fresh after the 2.5 check is
/// counted here too: "refused at 2.6", not "a forge call was spent".
pub(crate) const REFUSED_PROBED: &str = "guard.open_pr.refused_probed";
/// The first refusal of an `(issue, PR)` pair in a workspace this UTC hour.
pub(crate) const REFUSED_DISTINCT: &str = "guard.open_pr.refused_distinct";

impl SweepRegistry {
    /// The 2.5-position zero-cost refusal (#7606): the fresh memo entry for
    /// `issue`, when this is not a resume. A resume never consults the memo.
    pub(crate) fn dispatch_open_pr_memo(
        &self,
        issue: u32,
        resume_bypass_pr: Option<u32>,
    ) -> Option<OpenPrMemoEntry> {
        if resume_bypass_pr.is_some() {
            return None;
        }
        let memo = self.fresh_open_pr_memo(issue, Utc::now())?;
        self.note_open_pr_refusal(issue, memo.pr, REFUSED_MEMO);
        Some(memo)
    }

    /// The 2.6 probe: live for a resume, memo-aware otherwise. Records a
    /// refusal in the ledger when the answer will refuse this dispatch.
    pub(crate) fn dispatch_open_pr_probe(
        &self,
        issue: u32,
        resume_bypass_pr: Option<u32>,
    ) -> OpenPrProbe {
        let verdict = if resume_bypass_pr.is_some() {
            self.live_open_pr_probe(issue)
        } else {
            self.probe_open_linked_pr(issue)
        };
        if let OpenPrProbe::Open(pr) = verdict {
            if resume_bypass_pr != Some(pr) {
                self.note_open_pr_refusal(issue, pr, REFUSED_PROBED);
            }
        }
        verdict
    }

    /// Where the work finder's batched open-PR pre-filter (#8865) reads the
    /// listing: this registry's `gh`, its workspace root, and the `owner/repo`
    /// the 2.6 guard resolves ([`Self::resolve_owner_repo`], never `origin`'s)
    /// — the same three inputs `probe_open_linked_pr_transports` hands leg 0,
    /// so the pre-filter and the guard read one listing of one repo.
    ///
    /// `None` when the guards are off (`skip_label_flip`, mirroring 2.5/2.6) or
    /// the repo cannot be resolved; the pre-filter then drops nothing.
    ///
    /// The caller takes this under the registry lock and reads the listing
    /// after releasing it, so the listing read itself never runs under the
    /// mutex. Resolving the repo is not free of forge calls, though:
    /// `resolve_owner_repo` re-verifies the repo-facts record over `gh` when
    /// it is past its verify interval, and runs `gh repo view` on a cold or
    /// failed resolve (#9572 caches only a success). Either can run under the
    /// registry mutex here. That is the existing #9573 class, the same call
    /// every guard step already makes, bounded and at most once per workspace
    /// per tick on this path.
    pub(crate) fn open_pr_listing_target(&self) -> Option<(PathBuf, PathBuf, String)> {
        if self.config.skip_label_flip {
            return None;
        }
        let (owner, repo) = self.resolve_owner_repo()?;
        Some((
            self.resolved_gh(),
            self.config.workspace_root.clone(),
            format!("{owner}/{repo}"),
        ))
    }

    /// Fold the pre-filter's verified listing verdicts into the #6788 memo
    /// (#8865), as [`Self::live_open_pr_probe`] does for the one issue it
    /// probes. The pre-filter resolves a PR-blocked candidate before
    /// `dispatch()`, so the 2.6 probe no longer runs for it and would never
    /// write its entry; without this the known-PR recheck backstop has no PR
    /// to recheck on the tick every transport fails.
    ///
    /// `Open(pr)` records or refreshes the entry, dated `read_started`.
    /// `NoneOpen` removes it, so a PR that closed frees its issue on this
    /// tick instead of being refused at 2.5 for the rest of
    /// [`OPEN_PR_MEMO_FRESH`]. Removing an entry is not a permit: 2.6 still
    /// probes. An entry verified after `read_started` is newer evidence than
    /// this read (the listing is read with the registry unlocked, so a reap
    /// or another dispatch route can write one meanwhile) and is left alone.
    /// No forge call; a no-op when the memo is disabled.
    pub(crate) fn fold_listing_verdicts(
        &self,
        read_started: DateTime<Utc>,
        verdicts: &[(u32, OpenPrProbe)],
    ) {
        if !open_pr_memo_enabled() {
            return;
        }
        let mut memo = match self.open_pr_memo.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        for &(issue, verdict) in verdicts {
            if memo
                .get(&issue)
                .is_some_and(|entry| entry.verified_at > read_started)
            {
                continue;
            }
            match verdict {
                OpenPrProbe::Open(pr) => {
                    let verified_at = read_started;
                    memo.insert(issue, OpenPrMemoEntry { pr, verified_at });
                }
                OpenPrProbe::NoneOpen => {
                    memo.remove(&issue);
                }
                OpenPrProbe::ProbeFailed => {}
            }
        }
    }

    fn note_open_pr_refusal(&self, issue: u32, pr: u32, source: &'static str) {
        crate::forge_call_stats::counters::bump(source);
        let key = (self.config.workspace_root.clone(), issue, pr);
        let first = ledger()
            .lock()
            .map(|mut l| l.first_this_hour(key, Utc::now()))
            .unwrap_or(false);
        if first {
            crate::forge_call_stats::counters::bump(REFUSED_DISTINCT);
        }
    }
}

/// `(workspace root, issue, PR)` pairs refused in the current UTC hour.
#[derive(Debug, Default)]
pub(crate) struct HourLedger {
    hour: i64,
    seen: HashSet<(PathBuf, u32, u32)>,
}

impl HourLedger {
    /// Whether `key` is new in `now`'s UTC hour; records it. A new hour
    /// starts an empty set, so memory is bounded by one hour's distinct
    /// candidates.
    pub(crate) fn first_this_hour(&mut self, key: (PathBuf, u32, u32), now: DateTime<Utc>) -> bool {
        let hour = now.timestamp().div_euclid(3600);
        if hour != self.hour {
            self.hour = hour;
            self.seen.clear();
        }
        self.seen.insert(key)
    }
}

fn ledger() -> &'static Mutex<HourLedger> {
    static LEDGER: OnceLock<Mutex<HourLedger>> = OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(HourLedger::default()))
}

#[cfg(test)]
#[path = "refusal_memo_tests.rs"]
mod tests;
