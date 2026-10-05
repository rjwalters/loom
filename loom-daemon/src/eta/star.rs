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
//! (link) or issue-events coverage before `cutoff`: a lack of coverage is not
//! "unstarred".
//!
//! # Not a model input
//!
//! The result is recorded as the non-model fields `starred_any` and
//! `star_source`. The model's `starred` feature stays PR-only (see
//! `defaults/docs/eta.md`), so coefficient files are unchanged.

use super::fleet_events::{EventKind, ItemKind, RawEvent};
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
}

impl RepoStar {
    /// Read a repo's raw events (any order).
    #[must_use]
    pub fn from_events(events: &[RawEvent]) -> Self {
        let mut star = RepoStar::default();
        for e in events {
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
        let covered = |from: Option<DateTime<Utc>>| from.is_some_and(|f| f < cutoff);
        if !covered(self.links_from) || !covered(self.issue_events_from) {
            return None;
        }
        let links = self.links.get(&pr).map_or(&[][..], Vec::as_slice);
        Some(star_state_at(Some(pr_mask), pr_since, links, &self.issue_stars, cutoff))
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
    /// Read the raw event cache of each of `repos` under `root`.
    #[must_use]
    pub fn load(root: &std::path::Path, repos: &[String]) -> Self {
        let mut inputs = StarInputs::default();
        for repo in repos {
            let events =
                super::fleet_events::load_events(&super::fleet_events::events_path(root, repo));
            if !events.is_empty() {
                inputs
                    .repos
                    .insert(repo.to_ascii_lowercase(), RepoStar::from_events(&events));
            }
        }
        inputs
    }
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
