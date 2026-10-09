//! The serving side of "starred at `t`" (#10372).
//!
//! A pass hands the tracker, per repo, the links of every listed PR
//! (`linkage_refs` of its body, the work finder's rule) and the open issues
//! carrying a star label (one ETag-conditional listing per star label, the
//! work finder's own URLs, every page: #10389). The book stamps every
//! observation with the pass's instant and keeps the history: a link is known
//! from the first pass that saw it, a star from the first pass that listed the
//! issue, an unstar from the first pass that did not. [`star_state_at`] reads
//! it with `cutoff = as_of`, so an observation at or after `as_of` is never
//! used.
//!
//! A repo whose star listing failed on a pass is not updated by it
//! (`starred = None`): coverage is not falsity. A repo with no successful
//! observation before `as_of`, or whose last one is older than
//! [`STAR_MAX_AGE_SEC`], records nothing.
//!
//! The result is recorded as `starred_any` / `star_source`; the model's
//! `starred` is untouched.

use super::{Item, ItemKey, Tracker};
use crate::eta::explanation::Features;
use crate::eta::labels::pr_flags;
use crate::eta::star::{linked_star_at, star_state_at, IssueStarChange, LinkedStar, StarLink};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, BTreeSet};

/// A repo's last successful star observation older than this is stale.
pub const STAR_MAX_AGE_SEC: i64 = 3600;

#[derive(Debug, Clone, Default)]
struct RepoBook {
    /// First successful observation.
    first_at: Option<DateTime<Utc>>,
    /// Last successful observation.
    last_at: Option<DateTime<Utc>>,
    /// PR -> issue -> first pass that saw the link.
    links: BTreeMap<u32, BTreeMap<u32, DateTime<Utc>>>,
    /// Star changes by issue, in observation order.
    changes: BTreeMap<u32, Vec<IssueStarChange>>,
    /// Issues starred on the last observation.
    current: BTreeSet<u32>,
}

/// Per-repo (lowercased) observations.
#[derive(Debug, Clone, Default)]
pub(super) struct StarBook {
    repos: BTreeMap<String, RepoBook>,
}

impl Tracker {
    /// One repo's star observation at `now`: `links` (PR number, the issues
    /// its body links) for every listed PR, and `starred` (the open issues
    /// carrying a star label), or `None` when that listing failed.
    pub fn on_star_context(
        &mut self,
        repo: &str,
        links: &[(u32, Vec<u32>)],
        starred: Option<&[u32]>,
        now: DateTime<Utc>,
    ) {
        let Some(starred) = starred else {
            return;
        };
        let book = self
            .star
            .repos
            .entry(repo.to_ascii_lowercase())
            .or_default();
        book.first_at.get_or_insert(now);
        book.last_at = Some(now);
        let now_starred: BTreeSet<u32> = starred.iter().copied().collect();
        for issue in now_starred.difference(&book.current) {
            book.changes
                .entry(*issue)
                .or_default()
                .push(IssueStarChange {
                    issue: *issue,
                    at: now,
                    starred: true,
                });
        }
        for issue in book.current.difference(&now_starred) {
            book.changes
                .entry(*issue)
                .or_default()
                .push(IssueStarChange {
                    issue: *issue,
                    at: now,
                    starred: false,
                });
        }
        book.current = now_starred;
        // Links of PRs no longer listed are dropped; a PR's links are kept
        // from the first pass that saw each.
        let listed: BTreeSet<u32> = links.iter().map(|(pr, _)| *pr).collect();
        book.links.retain(|pr, _| listed.contains(pr));
        for (pr, issues) in links {
            let seen = book.links.entry(*pr).or_default();
            for issue in issues {
                seen.entry(*issue).or_insert(now);
            }
        }
        let linked: BTreeSet<u32> = book
            .links
            .values()
            .flat_map(|m| m.keys().copied())
            .collect();
        book.changes
            .retain(|issue, _| book.current.contains(issue) || linked.contains(issue));
    }

    /// Record the PR's star state at `now` on `features`; nothing when the
    /// item has no PR or the repo has no fresh observation before `now`.
    pub(super) fn star_features(
        &self,
        key: &ItemKey,
        item: &Item,
        now: DateTime<Utc>,
        features: &mut Features,
    ) {
        let Some(pr) = item.pr_number.filter(|_| !item.labels.is_empty()) else {
            return;
        };
        let Some((links, changes)) = self.star_inputs(&key.repo, pr, now) else {
            return;
        };
        let state = star_state_at(Some(pr_flags(&item.labels)), None, &links, &changes, now);
        features.starred_any = Some(state.source.starred());
        features.star_source = Some(state.source.as_str().to_string());
    }

    /// `pr`'s linked-issue star at `now`, for the priority features (#10333):
    /// its current run and on/off instants. Empty when the repo has no fresh
    /// observation before `now`.
    pub(super) fn linked_star(&self, repo: &str, pr: u32, now: DateTime<Utc>) -> LinkedStar {
        self.linked_star_known(repo, pr, now).unwrap_or_default()
    }

    /// [`Self::linked_star`], but `None` (unknown) when the repo has no fresh
    /// observation before `now`, for the v2 priority inputs (#10508).
    pub(super) fn linked_star_known(
        &self,
        repo: &str,
        pr: u32,
        now: DateTime<Utc>,
    ) -> Option<LinkedStar> {
        self.star_inputs(repo, pr, now)
            .map(|(links, changes)| linked_star_at(&links, &changes, now))
    }

    /// `pr`'s links and every star change of `repo` (lowercased), when the
    /// repo has a fresh observation before `now`.
    fn star_inputs(
        &self,
        repo: &str,
        pr: u32,
        now: DateTime<Utc>,
    ) -> Option<(Vec<StarLink>, Vec<IssueStarChange>)> {
        let book = self.star.repos.get(repo)?;
        let fresh = book.first_at.is_some_and(|f| f < now)
            && book
                .last_at
                .is_some_and(|l| (now - l).num_seconds() <= STAR_MAX_AGE_SEC);
        if !fresh {
            return None;
        }
        let links: Vec<StarLink> = book
            .links
            .get(&pr)
            .into_iter()
            .flatten()
            .map(|(issue, known_at)| StarLink {
                issue: *issue,
                known_at: *known_at,
            })
            .collect();
        let changes: Vec<IssueStarChange> = book.changes.values().flatten().copied().collect();
        Some((links, changes))
    }
}
