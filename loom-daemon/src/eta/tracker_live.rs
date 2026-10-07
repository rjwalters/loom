//! The tracker's live items as plain values (Issue #10196, `fleet.state`).
//!
//! A read-only view. It clones what the tracker already holds in memory and
//! never touches the estimation path, the forge or the clock, so the
//! `fleet.state` emitter is a reader of tracker state and not a second tick
//! loop. An item is live when it has a current stage, has not landed, and is
//! still a member of the fleet's in-flight work: a running sweep, a PR in a
//! review listing, or a ready-queue row. An item that only awaits an outcome
//! read (its sweep ended, or its PR left the listing) keeps its stage for
//! the pending ETA outcomes but is not current state, so it is not exported.
//!
//! A listed PR whose body only says `Part of #N` is never a tracker item (the
//! tracker keys PRs by the issue they close), yet it is in the fleet's roster
//! and census. It is surfaced here from the roster and the pass's PR links, so
//! a slice PR keeps its issue, PR, stage and entry instant after its sweep ends.

use super::Tracker;
use crate::eta::{AgeSource, Stage};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, BTreeSet};

/// One live item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveItem {
    /// Lowercased `owner/repo`.
    pub repo: String,
    /// Issue.
    pub issue: u32,
    /// Current stage.
    pub stage: Stage,
    /// When it entered `stage`.
    pub entered_at: DateTime<Utc>,
    /// `entered_at` is a lower bound, not an observed transition.
    pub entered_at_lower_bound: bool,
    /// The PR, when known.
    pub pr: Option<u32>,
    /// The sweep this host runs for it, while that sweep is running.
    pub running_sweep_id: Option<String>,
}

impl Tracker {
    /// Every live item, in `(repo, issue)` order.
    #[must_use]
    pub fn live_items(&self) -> Vec<LiveItem> {
        let mut live = self.tracked_live_items();
        live.extend(self.listed_slice_items(&live));
        live.sort_by(|a, b| (&a.repo, a.issue).cmp(&(&b.repo, b.issue)));
        live
    }

    /// The pass's linkage for `repo`'s listed PRs, as `(PR number, issues its
    /// body links)`. Replaces that repo's previous links; a PR that left the
    /// listing drops out.
    pub fn on_pr_links(&mut self, repo: &str, links: &[(u32, Vec<u32>)]) {
        let repo = repo.to_ascii_lowercase();
        self.context.pr_links.retain(|(r, _), _| *r != repo);
        for (pr, issues) in links {
            self.context
                .pr_links
                .insert((repo.clone(), *pr), issues.clone());
        }
    }

    /// Listed PRs no tracker item follows, keyed to the first issue their body
    /// links. A PR with no link, or whose issue already has a row, adds none.
    /// Of several PRs for one issue the lowest number is kept.
    fn listed_slice_items(&self, live: &[LiveItem]) -> Vec<LiveItem> {
        let Some(view) = &self.context.fleet else {
            return Vec::new();
        };
        let followed: BTreeSet<(&str, u32)> = self
            .items
            .iter()
            .filter_map(|(key, item)| Some((key.repo.as_str(), item.pr_number?)))
            .collect();
        let mut rows: BTreeMap<(&str, u32), LiveItem> = BTreeMap::new();
        for entry in &view.roster {
            let Some(stage) = entry.stage else { continue };
            if followed.contains(&(entry.repo.as_str(), entry.pr)) {
                continue;
            }
            let Some(issue) = self
                .context
                .pr_links
                .get(&(entry.repo.clone(), entry.pr))
                .and_then(|issues| issues.first().copied())
            else {
                continue;
            };
            if live
                .iter()
                .any(|l| l.repo == entry.repo && l.issue == issue)
            {
                continue;
            }
            let row = LiveItem {
                repo: entry.repo.clone(),
                issue,
                stage,
                entered_at: entry.entered_at,
                // Dated from the label timeline or `updated_at`, never from an
                // observed transition.
                entered_at_lower_bound: true,
                pr: Some(entry.pr),
                running_sweep_id: None,
            };
            match rows.entry((entry.repo.as_str(), issue)) {
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    if row.pr < slot.get().pr {
                        slot.insert(row);
                    }
                }
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(row);
                }
            }
        }
        rows.into_values().collect()
    }

    fn tracked_live_items(&self) -> Vec<LiveItem> {
        self.items
            .iter()
            .filter(|(_, item)| {
                !item.landed
                    && (item.sweep_running || item.in_review_listing || item.in_ready_queue)
            })
            .filter_map(|(key, item)| {
                let track = item.stage.as_ref()?;
                Some(LiveItem {
                    repo: key.repo.clone(),
                    issue: key.issue,
                    stage: track.stage,
                    entered_at: track.entered_at,
                    entered_at_lower_bound: !track.exact
                        || track.source == AgeSource::UpdatedAtLowerBound,
                    pr: item.pr_number,
                    running_sweep_id: if item.sweep_running {
                        item.sweep_id.clone()
                    } else {
                        None
                    },
                })
            })
            .collect()
    }
}
