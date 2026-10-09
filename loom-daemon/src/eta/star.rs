//! The one definition of "starred" for a PR at an instant (#10372).
//!
//! The operator's star (`loom:operator-priority`, #9244) is almost always put
//! on the **issue**; the work finder treats a PR as starred through the issue
//! it links. [`labels::pr_flags`] sees only the PR's own labels, so the model's
//! `starred` input misses most starred work. [`star_state_at`] decides, for a PR
//! strictly before `cutoff`:
//!
//! > the PR's own mask in force (`pr_flags & FLAG_STARRED`), **or** any issue it
//! > links whose link **and** star were both known before `cutoff`.
//!
//! Training ([`crate::eta::fit::rows`], `cutoff = t - LAG`) and serving
//! ([`crate::eta::tracker`], `cutoff = as_of`) both call it; no other ETA module
//! decides a star from `loom:operator-priority`.
//!
//! # Link rule
//!
//! [`crate::worktree_ops::gh::linkage_refs`]: every linked issue, closing
//! keywords and `Part of` / `Contributes to`. That is the work finder's rule.
//! Training reads the raw event cache's `closing_ref` rows (stamped at the PR's
//! `created_at`, see `fleet_events_pulls`); serving reads the listed PR's body
//! and stamps a link at the pass that first saw it.
//!
//! # Knowable-at
//!
//! A fact at instant `a` is usable iff `a < cutoff`. A star run counts from the
//! later of its labeled-at time and the link's known-at time, so a link that
//! becomes known only after `T` never stars a row at `T`.
//!
//! # Unknown coverage
//!
//! [`RepoStar::state_at`] returns `None` when the repo's cache has no pulls
//! (link) or issue-events coverage before `cutoff`, **or** when `cutoff` is
//! after the instant the cache was last known caught up
//! ([`RepoStar::synced_through`], from the listings' cursor stamps): a lack of
//! coverage is not "unstarred". A cache that stops advancing (for one, while
//! SigNoz covers its repo, #10520) therefore turns unknown after its stamp
//! rather than reading every later link or star as absent.
//!
//! # Not a model input
//!
//! The result is recorded as the non-model fields `starred_any` and
//! `star_source`. The model's `starred` feature stays PR-only (see
//! `defaults/docs/eta.md`), so coefficient files are unchanged.

use super::fleet_events::{EventKind, ItemKind, RawEvent, SOURCE_FORGE};
use super::fleet_signoz_history::SOURCE_SIGNOZ;
use super::labels::FLAG_STARRED;
use crate::worktree_ops::gh::linkage_refs;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

/// A PR's link to an issue, known from `known_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StarLink {
    /// The linked issue.
    pub issue: u32,
    /// When the link became knowable.
    pub known_at: DateTime<Utc>,
}

/// An issue's star turning on or off at `at` (when it became knowable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IssueStarChange {
    /// The issue.
    pub issue: u32,
    /// When the change became knowable.
    pub at: DateTime<Utc>,
    /// Starred from `at` on (`false`: the star was removed).
    pub starred: bool,
}

/// Where a PR's star comes from.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum StarSource {
    /// Not starred.
    #[default]
    None,
    /// The PR's own labels only.
    Pr,
    /// A linked issue only.
    Issue,
    /// Both.
    Both,
}

impl StarSource {
    /// Whether the PR counts as starred.
    #[must_use]
    pub fn starred(self) -> bool {
        self != StarSource::None
    }

    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            StarSource::None => "none",
            StarSource::Pr => "pr",
            StarSource::Issue => "issue",
            StarSource::Both => "both",
        }
    }
}

/// A PR's star at an instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StarState {
    /// Where the star comes from.
    pub source: StarSource,
    /// Start of the current uninterrupted star run: the earliest known start
    /// among the sources still active. `None` when not starred, or when the
    /// only active source is the PR's own and its start was not supplied.
    pub since: Option<DateTime<Utc>>,
}

/// Whether `label` is a star label, at any level. The only place the ETA
/// code classifies an issue label as a star.
#[must_use]
pub fn is_star_label(label: &str) -> bool {
    crate::operator_levels::is_starred(&[label])
}

/// The one definition of "starred" for a PR strictly before `cutoff` (#10372).
///
/// - `pr_mask`: the PR's [`super::labels::pr_flags`] in force; `pr_since` the
///   start of its current star run when known.
/// - `links`: the PR's links; `issue_stars`: star changes of any issues.
///
/// An issue counts iff a link to it is known before `cutoff`, and replaying
/// its changes known before `cutoff` leaves it starred. Its run starts at the
/// later of the run's start and the earliest such link's `known_at`.
#[must_use]
pub fn star_state_at(
    pr_mask: Option<u8>,
    pr_since: Option<DateTime<Utc>>,
    links: &[StarLink],
    issue_stars: &[IssueStarChange],
    cutoff: DateTime<Utc>,
) -> StarState {
    let pr_active = pr_mask.is_some_and(|m| m & FLAG_STARRED != 0);
    let mut linked: BTreeMap<u32, DateTime<Utc>> = BTreeMap::new();
    for link in links.iter().filter(|l| l.known_at < cutoff) {
        let slot = linked.entry(link.issue).or_insert(link.known_at);
        *slot = (*slot).min(link.known_at);
    }
    let mut changes: Vec<&IssueStarChange> = issue_stars
        .iter()
        .filter(|c| c.at < cutoff && linked.contains_key(&c.issue))
        .collect();
    changes.sort_by_key(|c| (c.at, c.starred));
    // issue -> start of the current run
    let mut runs: BTreeMap<u32, DateTime<Utc>> = BTreeMap::new();
    for change in changes {
        if change.starred {
            runs.entry(change.issue).or_insert(change.at);
        } else {
            runs.remove(&change.issue);
        }
    }
    let issue_since = runs
        .iter()
        .filter_map(|(issue, start)| linked.get(issue).map(|known| (*start).max(*known)))
        .min();
    let issue_active = issue_since.is_some();
    let source = match (pr_active, issue_active) {
        (false, false) => StarSource::None,
        (true, false) => StarSource::Pr,
        (false, true) => StarSource::Issue,
        (true, true) => StarSource::Both,
    };
    let since = [pr_active.then_some(pr_since).flatten(), issue_since]
        .into_iter()
        .flatten()
        .min();
    StarState { source, since }
}

/// A PR's linked-issue star strictly before a cutoff (#10333): the current
/// run and its history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkedStar {
    /// Start of the current linked-issue star run, as [`star_state_at`] gives
    /// it for the issues alone; `None` when no linked issue is starred.
    pub since: Option<DateTime<Utc>>,
    /// Every instant the verdict "a linked issue stars the PR" turned on or
    /// off, ascending. An unstar ends the run and leaves `since` empty, so
    /// this is the only record that a run started and ended.
    pub changes: Vec<DateTime<Utc>>,
}

/// [`star_state_at`] for a PR's linked issues alone, with the instants its
/// verdict flipped (#10333). Each flip is read off [`star_state_at`] itself,
/// just after each fact instant, so it uses the same link and run rules.
#[must_use]
pub fn linked_star_at(
    links: &[StarLink],
    issue_stars: &[IssueStarChange],
    cutoff: DateTime<Utc>,
) -> LinkedStar {
    let state = |c: DateTime<Utc>| star_state_at(Some(0), None, links, issue_stars, c);
    let known: Vec<&StarLink> = links.iter().filter(|l| l.known_at < cutoff).collect();
    let mut instants: Vec<DateTime<Utc>> = known
        .iter()
        .map(|l| l.known_at)
        .chain(
            issue_stars
                .iter()
                .filter(|c| c.at < cutoff && known.iter().any(|l| l.issue == c.issue))
                .map(|c| c.at),
        )
        .collect();
    instants.sort();
    instants.dedup();
    let mut on = false;
    let mut changes = Vec::new();
    for at in instants {
        // Facts at `at` are known strictly before `at + 1ns`, which is never
        // past `cutoff`.
        let now_on = state(at + chrono::Duration::nanoseconds(1))
            .source
            .starred();
        if now_on != on {
            changes.push(at);
            on = now_on;
        }
    }
    let current = state(cutoff);
    LinkedStar {
        since: current.source.starred().then_some(current.since).flatten(),
        changes,
    }
}

/// One repo's star inputs for the fit, read from its raw event cache.
#[derive(Debug, Clone, Default)]
pub struct RepoStar {
    /// Links by PR, from `closing_ref` rows.
    pub links: BTreeMap<u32, Vec<StarLink>>,
    /// Star changes of every issue, in canonical order.
    pub issue_stars: Vec<IssueStarChange>,
    /// The earliest `closing_ref` row: the pulls listing's coverage.
    pub links_from: Option<DateTime<Utc>>,
    /// The earliest issue row: the issue-events listing's coverage.
    pub issue_events_from: Option<DateTime<Utc>>,
    /// The cache is complete through this instant (the earliest of its
    /// listings' [`super::fleet_events::EndpointCursor::synced_through`]); a
    /// cutoff after it is uncovered. `None`: never stamped, no upper bound.
    pub synced_through: Option<DateTime<Utc>>,
}

impl RepoStar {
    /// Read a repo's raw events (any order). Only [`SOURCE_FORGE`] rows set
    /// coverage: the coverage floors name the forge listings, and an imported
    /// webhook-mirror row (#10197) would move them and replay a star twice.
    /// A [`SOURCE_SIGNOZ`] row (#10746) is a star change only: it sets no
    /// floor and no stamp, so it counts only where the forge listings cover.
    #[must_use]
    pub fn from_events(events: &[RawEvent]) -> Self {
        let mut star = RepoStar::default();
        for e in events {
            if e.source == SOURCE_SIGNOZ {
                // A star change SigNoz dated (#10746): a star change only. It
                // sets no coverage floor; the forge listings' rows do.
                let label = e.label.as_deref().filter(|l| is_star_label(l));
                if let (ItemKind::Issue, Some(_), EventKind::LabelAdded | EventKind::LabelRemoved) =
                    (e.item_kind, label, e.kind)
                {
                    star.issue_stars.push(IssueStarChange {
                        issue: e.item,
                        at: e.event_time,
                        starred: e.kind == EventKind::LabelAdded,
                    });
                }
                continue;
            }
            if e.source != SOURCE_FORGE {
                continue;
            }
            let min = |slot: &mut Option<DateTime<Utc>>| {
                *slot = Some(slot.map_or(e.event_time, |x| x.min(e.event_time)));
            };
            match (e.item_kind, e.kind) {
                (ItemKind::Pr, EventKind::ClosingRef) => {
                    min(&mut star.links_from);
                    if let Some(issue) = e.target {
                        star.links.entry(e.item).or_default().push(StarLink {
                            issue,
                            known_at: e.event_time,
                        });
                    }
                }
                (ItemKind::Issue, kind) => {
                    min(&mut star.issue_events_from);
                    let label = e.label.as_deref().filter(|l| is_star_label(l));
                    if let (Some(_), EventKind::LabelAdded | EventKind::LabelRemoved) =
                        (label, kind)
                    {
                        star.issue_stars.push(IssueStarChange {
                            issue: e.item,
                            at: e.event_time,
                            starred: kind == EventKind::LabelAdded,
                        });
                    }
                }
                _ => {}
            }
        }
        star.issue_stars.sort_by_key(|c| (c.at, c.issue, c.starred));
        star
    }

    /// [`star_state_at`] for `pr`, or `None` when the cache does not cover
    /// `cutoff` (unknown, never "unstarred").
    #[must_use]
    pub fn state_at(
        &self,
        pr: u32,
        pr_mask: u8,
        pr_since: Option<DateTime<Utc>>,
        cutoff: DateTime<Utc>,
    ) -> Option<StarState> {
        let links = self.covered_links(pr, cutoff)?;
        Some(star_state_at(Some(pr_mask), pr_since, links, &self.issue_stars, cutoff))
    }

    /// [`linked_star_at`] for `pr`, or `None` when the cache does not cover
    /// `cutoff`.
    #[must_use]
    pub fn linked_at(&self, pr: u32, cutoff: DateTime<Utc>) -> Option<LinkedStar> {
        let links = self.covered_links(pr, cutoff)?;
        Some(linked_star_at(links, &self.issue_stars, cutoff))
    }

    /// `pr`'s links, when both listings cover `cutoff`: from below (a row
    /// before it) and from above (synced through it, when stamped).
    fn covered_links(&self, pr: u32, cutoff: DateTime<Utc>) -> Option<&[StarLink]> {
        let covered = |from: Option<DateTime<Utc>>| from.is_some_and(|f| f < cutoff);
        if !covered(self.links_from) || !covered(self.issue_events_from) {
            return None;
        }
        if self.synced_through.is_some_and(|through| cutoff > through) {
            return None;
        }
        Some(self.links.get(&pr).map_or(&[][..], Vec::as_slice))
    }
}

/// The star inputs of every repo, by lowercased `owner/repo`. A repo with no
/// entry is uncovered.
#[derive(Debug, Clone, Default)]
pub struct StarInputs {
    /// Per repo.
    pub repos: BTreeMap<String, RepoStar>,
}

impl StarInputs {
    /// Read the raw event cache of each of `repos` under `root`, bounded
    /// above by its cursor's stamps ([`listing_keys`]).
    #[must_use]
    pub fn load(root: &std::path::Path, repos: &[String]) -> Self {
        use super::fleet_events::{cursor_path, events_path, load_events, EventsCursor};
        let mut inputs = StarInputs::default();
        for repo in repos {
            let events = load_events(&events_path(root, repo));
            if !events.is_empty() {
                let mut star = RepoStar::from_events(&events);
                star.synced_through = EventsCursor::read(&cursor_path(root, repo), repo)
                    .synced_through(&listing_keys());
                inputs.repos.insert(repo.to_ascii_lowercase(), star);
            }
        }
        inputs
    }
}

/// The cursor keys of the repo-wide listings star coverage reads: issue
/// events and pulls (links).
#[must_use]
pub fn listing_keys() -> Vec<String> {
    use super::fleet_events_forge::ForgeEndpoint;
    ForgeEndpoint::ALL
        .into_iter()
        .filter(|e| e.per_pr().is_none())
        .map(|e| format!("{SOURCE_FORGE}:{}", e.name()))
        .collect()
}

/// The linked issues of a PR body, by the work finder's rule.
#[must_use]
pub fn body_links(body: &str) -> Vec<u32> {
    linkage_refs(body).into_iter().map(|(n, _)| n).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::eta::labels::FLAG_OP_HOLD;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_790_000_000 + secs, 0).unwrap()
    }

    fn link(issue: u32, secs: i64) -> StarLink {
        StarLink {
            issue,
            known_at: at(secs),
        }
    }

    fn change(issue: u32, secs: i64, starred: bool) -> IssueStarChange {
        IssueStarChange {
            issue,
            at: at(secs),
            starred,
        }
    }

    const CUT: i64 = 1000;

    fn state(
        mask: Option<u8>,
        since: Option<i64>,
        links: &[StarLink],
        stars: &[IssueStarChange],
    ) -> StarState {
        star_state_at(mask, since.map(at), links, stars, at(CUT))
    }

    #[test]
    fn pr_only() {
        let s = state(Some(FLAG_STARRED), Some(10), &[], &[]);
        assert_eq!(
            s,
            StarState {
                source: StarSource::Pr,
                since: Some(at(10))
            }
        );
        let s = state(Some(FLAG_STARRED), None, &[], &[]);
        assert_eq!(
            s,
            StarState {
                source: StarSource::Pr,
                since: None
            }
        );
    }

    #[test]
    fn issue_only() {
        let s = state(Some(0), None, &[link(7, 10)], &[change(7, 20, true)]);
        assert_eq!(
            s,
            StarState {
                source: StarSource::Issue,
                since: Some(at(20))
            }
        );
    }

    #[test]
    fn both_takes_the_earliest_active_start() {
        let s = state(Some(FLAG_STARRED), Some(50), &[link(7, 10)], &[change(7, 20, true)]);
        assert_eq!(
            s,
            StarState {
                source: StarSource::Both,
                since: Some(at(20))
            }
        );
        let s = state(Some(FLAG_STARRED), Some(5), &[link(7, 10)], &[change(7, 20, true)]);
        assert_eq!(s.since, Some(at(5)));
    }

    #[test]
    fn none() {
        let s = state(Some(FLAG_OP_HOLD), None, &[link(7, 10)], &[]);
        assert_eq!(
            s,
            StarState {
                source: StarSource::None,
                since: None
            }
        );
        assert!(!s.source.starred());
        assert_eq!(state(None, None, &[], &[]).source, StarSource::None);
    }

    #[test]
    fn an_unstar_ends_the_run_and_a_restar_starts_a_new_one() {
        let links = [link(7, 10)];
        let ended = [change(7, 20, true), change(7, 30, false)];
        assert_eq!(state(Some(0), None, &links, &ended).source, StarSource::None);
        let again = [
            change(7, 20, true),
            change(7, 30, false),
            change(7, 40, true),
        ];
        assert_eq!(state(Some(0), None, &links, &again).since, Some(at(40)));
    }

    #[test]
    fn a_fact_at_exactly_the_cutoff_is_not_yet_known() {
        let links = [link(7, 10)];
        assert_eq!(state(Some(0), None, &links, &[change(7, CUT, true)]).source, StarSource::None);
        assert_eq!(
            state(Some(0), None, &links, &[change(7, CUT - 1, true)]).source,
            StarSource::Issue
        );
    }

    #[test]
    fn a_link_known_after_the_cutoff_does_not_star() {
        let stars = [change(7, 20, true)];
        assert_eq!(state(Some(0), None, &[link(7, CUT)], &stars).source, StarSource::None);
        assert_eq!(state(Some(0), None, &[link(7, CUT + 5)], &stars).source, StarSource::None);
        // A star older than the link counts from the link.
        let s = state(Some(0), None, &[link(7, 500)], &stars);
        assert_eq!(s.since, Some(at(500)));
    }

    #[test]
    fn any_of_several_linked_issues_stars() {
        let links = [link(7, 10), link(8, 10), link(9, 10)];
        let s = state(Some(0), None, &links, &[change(8, 20, true), change(9, 30, true)]);
        assert_eq!(
            s,
            StarState {
                source: StarSource::Issue,
                since: Some(at(20))
            }
        );
        // An unlinked issue's star is nothing.
        assert_eq!(
            state(Some(0), None, &[link(7, 10)], &[change(8, 20, true)]).source,
            StarSource::None
        );
    }

    #[test]
    fn linked_star_keeps_the_on_off_instants() {
        let links = [link(7, 10)];
        // Starred and unstarred: no current run, both instants kept.
        let ended = [change(7, 20, true), change(7, 30, false)];
        assert_eq!(
            linked_star_at(&links, &ended, at(CUT)),
            LinkedStar {
                since: None,
                changes: vec![at(20), at(30)]
            }
        );
        // A star older than the link turns on when the link is known.
        let early = [change(7, 5, true), change(7, 30, false)];
        assert_eq!(linked_star_at(&links, &early, at(CUT)).changes, vec![at(10), at(30)]);
        // Overlapping issues: the verdict stays on across the hand-off.
        let two = [link(7, 10), link(8, 10)];
        let stars = [
            change(7, 20, true),
            change(8, 25, true),
            change(7, 30, false),
        ];
        let s = linked_star_at(&two, &stars, at(CUT));
        assert_eq!((s.since, s.changes), (Some(at(25)), vec![at(20)]));
        // An unstar at the cutoff is not yet known; an unlinked issue is nothing.
        let at_cut = [change(7, 20, true), change(7, CUT, false)];
        assert_eq!(
            linked_star_at(&links, &at_cut, at(CUT)),
            LinkedStar {
                since: Some(at(20)),
                changes: vec![at(20)]
            }
        );
        assert_eq!(linked_star_at(&[link(9, 10)], &ended, at(CUT)), LinkedStar::default());
    }

    #[test]
    fn part_of_counts_as_a_link() {
        assert_eq!(body_links("Part of #12\n\nCloses #3"), vec![3, 12]);
        assert_eq!(body_links("Contributes to #5"), vec![5]);
    }

    fn raw(
        item: u32,
        kind: ItemKind,
        event: EventKind,
        label: Option<&str>,
        secs: i64,
    ) -> RawEvent {
        RawEvent::new(
            "o/r",
            item,
            kind,
            event,
            label.map(str::to_string),
            at(secs),
            "forge",
            1,
            at(secs),
        )
    }

    #[test]
    fn unknown_coverage_is_none_not_unstarred() {
        let events = vec![
            raw(7, ItemKind::Issue, EventKind::LabelAdded, Some("loom:operator-priority"), 20),
            raw(3, ItemKind::Pr, EventKind::ClosingRef, Some("closes"), 10).with_target(Some(7)),
        ];
        let star = RepoStar::from_events(&events);
        // Issue-events coverage starts at 20, links at 10.
        assert_eq!(star.state_at(3, 0, None, at(15)), None, "issue events not yet covered");
        assert_eq!(star.state_at(3, 0, None, at(5)), None, "links not yet covered");
        let covered = star.state_at(3, 0, None, at(CUT)).unwrap();
        assert_eq!(covered.source, StarSource::Issue);
        // A PR with no link row is covered and unstarred, not unknown.
        assert_eq!(star.state_at(4, 0, None, at(CUT)).unwrap().source, StarSource::None);
        // An empty repo cache: no coverage at all.
        assert_eq!(RepoStar::default().state_at(3, 0, None, at(CUT)), None);
    }

    /// #10520 (judge round 3): a raw cache that stopped syncing at `T` (e.g.
    /// while SigNoz covers its repo) must not read a cutoff after `T` as
    /// "unstarred" — a link or star added after `T` is simply missing from
    /// it. Before `T` the cache answers; after it, unknown.
    #[test]
    fn a_cutoff_after_the_cache_stopped_syncing_is_unknown_not_unstarred() {
        let events = vec![
            raw(9, ItemKind::Issue, EventKind::LabelAdded, Some("loom:blocked"), 20),
            raw(4, ItemKind::Pr, EventKind::ClosingRef, Some("closes"), 10).with_target(Some(9)),
        ];
        let mut star = RepoStar::from_events(&events);
        // Unstamped: no upper bound (a pre-#10520 cache) — unchanged.
        assert_eq!(star.state_at(3, 0, None, at(CUT)).unwrap().source, StarSource::None);
        // Synced through 100; PR 3's link to a starred issue lands at 150,
        // after the cache stopped, so it is not in the cache.
        star.synced_through = Some(at(100));
        assert_eq!(star.state_at(3, 0, None, at(100)).unwrap().source, StarSource::None);
        assert_eq!(star.state_at(3, 0, None, at(200)), None, "after T: unknown, not unstarred");
        assert_eq!(star.linked_at(3, at(200)), None);
        assert!(star.linked_at(3, at(50)).is_some());
    }

    /// [`StarInputs::load`] bounds each repo by the earliest stamp of its two
    /// repo-wide listings, and leaves an unstamped cache unbounded.
    #[test]
    fn load_bounds_coverage_by_the_listings_synced_through() {
        use crate::eta::fleet_events::{self, EventLog, EventsCursor};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let repo = "o/r".to_string();
        let events = vec![
            raw(9, ItemKind::Issue, EventKind::LabelAdded, Some("loom:blocked"), 20),
            raw(4, ItemKind::Pr, EventKind::ClosingRef, Some("closes"), 10).with_target(Some(9)),
        ];
        EventLog::open(&fleet_events::events_path(root, &repo))
            .unwrap()
            .append(&events)
            .unwrap();
        let unbounded = StarInputs::load(root, std::slice::from_ref(&repo));
        assert_eq!(unbounded.repos["o/r"].synced_through, None);

        let cursor_file = fleet_events::cursor_path(root, &repo);
        let keys = listing_keys();
        assert_eq!(keys, vec!["forge:issues-events".to_string(), "forge:pulls".to_string()]);
        fleet_events::mark_synced_through(&cursor_file, &repo, &keys[0], at(300)).unwrap();
        fleet_events::mark_synced_through(&cursor_file, &repo, &keys[1], at(100)).unwrap();
        // Never moved back.
        fleet_events::mark_synced_through(&cursor_file, &repo, &keys[1], at(50)).unwrap();
        assert_eq!(
            EventsCursor::read(&cursor_file, &repo).endpoints[&keys[1]].synced_through,
            Some(at(100))
        );
        let bounded = StarInputs::load(root, std::slice::from_ref(&repo));
        let star = &bounded.repos["o/r"];
        assert_eq!(star.synced_through, Some(at(100)), "the earliest listing stamp");
        assert!(star.state_at(3, 0, None, at(100)).is_some());
        assert_eq!(star.state_at(3, 0, None, at(101)), None);
    }

    #[test]
    fn only_star_labels_on_issues_are_star_changes() {
        let events = vec![
            raw(7, ItemKind::Issue, EventKind::LabelAdded, Some("loom:blocked"), 20),
            raw(7, ItemKind::Pr, EventKind::LabelAdded, Some("loom:operator-priority"), 21),
            raw(7, ItemKind::Issue, EventKind::LabelAdded, Some("loom:operator-priority"), 22),
        ];
        let star = RepoStar::from_events(&events);
        assert_eq!(star.issue_stars.len(), 1);
        assert_eq!(star.issue_stars[0].at, at(22));
    }

    /// No other ETA module decides a star from the star label (#10372): the
    /// label constants live in `labels.rs` and `operator_levels.rs`, and the
    /// decision here.
    #[test]
    fn no_other_eta_module_reads_the_star_label() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/eta");
        let mut offenders = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|n| n != "tests") {
                        stack.push(path);
                    }
                    continue;
                }
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                if !name.ends_with(".rs") || name == "star.rs" || name == "labels.rs" {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                // Doc and comment lines may name the label.
                let code = text.lines().any(|l| {
                    let l = l.trim_start();
                    !l.starts_with("//") && l.contains("\"loom:operator-priority\"")
                });
                if code && !name.ends_with("_tests.rs") {
                    offenders.push(name);
                }
            }
        }
        assert!(offenders.is_empty(), "star label read outside star.rs/labels.rs: {offenders:?}");
    }
}
