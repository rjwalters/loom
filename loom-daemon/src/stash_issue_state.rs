//! Telling a blind `stashes` run apart from one unreadable issue (#8876).
//!
//! `stash_retirement` is fail-closed: an issue whose state cannot be read
//! keeps its stash. That is right for one issue and misleading for all of
//! them. When the invoking shell's `gh` login cannot see the repo, every stash reads
//! `KEEP ... could not determine the state of issue #N`, the run exits 0, and
//! the command has silently done nothing. This module records what each
//! lookup answered so the caller can report that case once, as its own
//! condition, and fail the run.
//!
//! It lives outside `stash_retirement.rs` because that file is size-frozen
//! (`.loom/docs/file-size-policy.md`), and it changes no verdict: the
//! recorder forwards every answer untouched.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::stash_retirement::{IssueStateLookup, RetirementReport};

/// An [`IssueStateLookup`] that remembers each distinct issue's answer.
pub struct RecordingLookup<'a> {
    inner: &'a mut dyn IssueStateLookup,
    seen: BTreeMap<u64, Option<bool>>,
}

impl<'a> RecordingLookup<'a> {
    #[must_use]
    pub fn new(inner: &'a mut dyn IssueStateLookup) -> Self {
        Self {
            inner,
            seen: BTreeMap::new(),
        }
    }

    /// Every issue asked about so far, with the last answer it got.
    #[must_use]
    pub fn seen(&self) -> &BTreeMap<u64, Option<bool>> {
        &self.seen
    }
}

impl IssueStateLookup for RecordingLookup<'_> {
    fn is_closed(&mut self, issue: u64) -> Option<bool> {
        let answer = self.inner.is_closed(issue);
        self.seen.insert(issue, answer);
        answer
    }
}

/// What a run's issue-state lookups amounted to. Serialized as the
/// `issue_state` object of `stashes --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IssueStateSummary {
    /// `true` when at least one lookup was attempted and none was answered.
    pub unreadable_for_all: bool,
    /// Distinct issues looked up. A stash with no `issue=` token is never
    /// looked up, so it does not count here.
    pub lookups_attempted: usize,
    /// How many of those came back unknown.
    pub lookups_unknown: usize,
    /// Stashes kept only because their issue's state was unknown.
    pub stashes_unknown: usize,
}

impl IssueStateSummary {
    /// Summarize `seen` (from [`RecordingLookup::seen`]) over `reports`.
    #[must_use]
    pub fn new(reports: &[RetirementReport], seen: &BTreeMap<u64, Option<bool>>) -> Self {
        let lookups_attempted = seen.len();
        let lookups_unknown = seen.values().filter(|answer| answer.is_none()).count();
        let stashes_unknown = reports
            .iter()
            .filter(|r| {
                r.entry
                    .issue
                    .is_some_and(|issue| seen.get(&issue) == Some(&None))
            })
            .count();
        Self {
            unreadable_for_all: lookups_attempted > 0 && lookups_unknown == lookups_attempted,
            lookups_attempted,
            lookups_unknown,
            stashes_unknown,
        }
    }

    /// The one line that names a blind run, or `None` for any other run.
    #[must_use]
    pub fn blind_message(&self) -> Option<String> {
        self.unreadable_for_all.then(|| {
            format!(
                "issue state unreadable for all {stashes} stash(es): every lookup failed \
                 ({issues} distinct issue(s)), so the ambient gh login this command runs under \
                 cannot read this repo's issues. Nothing was retired and nothing can be until \
                 that is fixed.",
                stashes = self.stashes_unknown,
                issues = self.lookups_attempted,
            )
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
