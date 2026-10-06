//! The dispatch path's use of the verified-open-PR memo, and the open-PR
//! refusal ledger (W9 of the forge API reduction plan).
//!
//! # The memo serves refusals only, never a resume
//!
//! The #6788 memo ([`SweepRegistry::fresh_open_pr_memo`]) is in memory, fresh
//! for [`OPEN_PR_MEMO_FRESH`] (900 s) and never renewed: past that, the next
//! attempt runs the live probe, whose leg 0 (#10514) reads the ETag'd open-PR
//! listing (a free `304` on an unchanged repo). For an ordinary dispatch a
//! memo answer can only **refuse**: the 2.5 short circuit returns
//! `OpenPrDispatchError` without spending a call.
//!
//! A crash resume (#4256, `resume_bypass_pr = Some(pr)`) is the one place an
//! `Open(pr)` answer **permits** work: the reaper resumes because the probe
//! named a PR, and 2.6 then lets the dispatch through for that PR. So every
//! resume decision reads the forge live:
//!
//! - the reaper's eligibility probe calls [`SweepRegistry::live_open_pr_probe`];
//! - [`SweepRegistry::dispatch_open_pr_memo`] never short-circuits a resume;
//! - [`SweepRegistry::dispatch_open_pr_probe`] runs the live probe for one.
//!
//! The other memo readers (the reaper's #4366 no-progress exemption, the
//! PR-less retry tally and its `loom:blocked` hold veto) keep today's
//! fresh-only memo: each answer there is at most 900 s old, as before.
//!
//! # The ledger: distinct versus repeated refusals
//!
//! Every open-PR refusal bumps one of two `loom.forge.facade.events`
//! counters: [`REFUSED_MEMO`] (served by the memo, zero calls) or
//! [`REFUSED_PROBED`] (a live probe answered). A refusal of an
//! `(issue, PR)` pair this workspace has not refused yet in the current UTC
//! hour also bumps [`REFUSED_DISTINCT`]. Per hour, `distinct / (memo +
//! probed)` is the share of refusals that were new, which is what the plan's
//! open question 9 asks.

use super::*;
use std::collections::HashSet;

/// An open-PR refusal served by the fresh memo (no forge call).
pub(crate) const REFUSED_MEMO: &str = "guard.open_pr.refused_memo";
/// An open-PR refusal answered by a live probe.
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
