//! The per-repo limits `fleet.state` publishes (Issue #11606), read from a
//! real multi-workspace tick: the cap the tick resolved, each workspace's
//! occupancy when it finished, and the cap deferrals per workspace.

use std::collections::HashSet;
use std::path::PathBuf;

use super::super::*;

struct Source(Option<Vec<WorkItem>>);

impl Source {
    fn of(numbers: &[u32]) -> Self {
        Self(Some(
            numbers
                .iter()
                .map(|n| WorkItem::new(*n, vec!["loom:issue".to_string()]))
                .collect(),
        ))
    }
}

impl WorkSource for Source {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        Ok(self.0.take().unwrap_or_default())
    }
}

/// A repo already holding `live` sweeps; each admission adds one.
#[derive(Default)]
struct Repo {
    live: usize,
    dispatched: usize,
}

impl WorkDispatcher for Repo {
    fn in_flight(&self) -> HashSet<u32> {
        HashSet::new()
    }
    fn occupancy(&self) -> usize {
        self.live + self.dispatched
    }
    fn dispatch(&mut self, _issue: u32, _complexity: Option<&str>) -> Result<bool> {
        self.dispatched += 1;
        Ok(true)
    }
}

fn roots() -> Vec<PathBuf> {
    ["/src/a", "/src/b", "/src/idle"]
        .iter()
        .map(PathBuf::from)
        .collect()
}

/// Repo `a` is full (2 live), `b` has room (1 live), `idle` has no candidates.
fn tick(cap: Option<usize>, a_live: usize) -> (TickReport, crate::types::WorkFinderTickSummary) {
    let mut workspaces = [
        (
            Source::of(&[1, 2, 3]),
            Repo {
                live: a_live,
                dispatched: 0,
            },
        ),
        (
            Source::of(&[10]),
            Repo {
                live: 1,
                dispatched: 0,
            },
        ),
        (
            Source::of(&[]),
            Repo {
                live: 1,
                dispatched: 0,
            },
        ),
    ];
    let report = tick_multi_with_repo_cap(
        &mut workspaces,
        &[100, 100, 100],
        20usize.into(),
        &[false; 3],
        None,
        usize::MAX,
        false,
        None,
        cap,
        &[],
    );
    let summary = tick_summary(&report, 20, chrono::Utc::now(), &roots(), None);
    (report, summary)
}

fn limit<'a>(
    summary: &'a crate::types::WorkFinderTickSummary,
    root: &str,
) -> &'a crate::types::RepoLimitView {
    summary
        .repo_limits
        .iter()
        .find(|l| l.repo == root)
        .unwrap_or_else(|| panic!("no limits for {root}"))
}

/// A repo at its cap shows `in_flight == cap` and its deferrals; the repo
/// with room admits and shows its new occupancy; a repo with no candidates is
/// still named, with a measured zero.
#[test]
fn a_repo_at_its_cap_shows_in_flight_equal_to_cap_and_its_deferrals() {
    let (report, summary) = tick(Some(2), 2);
    assert_eq!(summary.repo_limits.len(), 3, "every workspace is named");
    let a = limit(&summary, "/src/a");
    assert_eq!((a.in_flight, a.cap, a.deferred_repo_cap), (2, Some(2), 3));
    let b = limit(&summary, "/src/b");
    assert_eq!((b.in_flight, b.cap, b.deferred_repo_cap), (2, Some(2), 0));
    let idle = limit(&summary, "/src/idle");
    assert_eq!((idle.in_flight, idle.cap, idle.deferred_repo_cap), (1, Some(2), 0));
    // The per-repo counts are the tick's own deferrals, not a recount.
    let total: usize = summary
        .repo_limits
        .iter()
        .map(|l| l.deferred_repo_cap)
        .sum();
    assert_eq!(total, report.deferred_repo_cap);
}

/// With no cap every repo reads `cap: None` (uncapped, observed) and nothing
/// is deferred, while the occupancy still shows.
#[test]
fn an_uncapped_tick_reports_a_null_cap_and_zero_deferrals() {
    let (_, summary) = tick(None, 2);
    assert_eq!(summary.repo_limits.len(), 3);
    for l in &summary.repo_limits {
        assert_eq!(l.cap, None, "{}", l.repo);
        assert_eq!(l.deferred_repo_cap, 0, "{}", l.repo);
    }
    // All three of repo a's candidates were admitted on top of its 2 live.
    assert_eq!(limit(&summary, "/src/a").in_flight, 5);
}

/// A changed cap or occupancy changes the published limits tick to tick.
#[test]
fn a_changed_cap_or_occupancy_changes_the_limits() {
    let (_, tight) = tick(Some(2), 2);
    let (_, roomy) = tick(Some(4), 2);
    let (_, drained) = tick(Some(2), 0);
    assert_ne!(tight.repo_limits, roomy.repo_limits);
    assert_eq!(limit(&roomy, "/src/a").deferred_repo_cap, 1);
    assert_eq!(limit(&roomy, "/src/a").in_flight, 4);
    // Room for two of three: the third is deferred, occupancy ends at the cap.
    let a = limit(&drained, "/src/a");
    assert_eq!((a.in_flight, a.deferred_repo_cap), (2, 1));
}

/// A summary built without workspace roots (the single-workspace tick) says
/// nothing about repos: empty means unknown, not uncapped.
#[test]
fn a_tick_without_roots_publishes_no_limits() {
    let (report, _) = tick(Some(2), 2);
    let summary = tick_summary(&report, 20, chrono::Utc::now(), &[], None);
    assert!(summary.repo_limits.is_empty());
    let single = TickReport {
        deferred_repo_cap: 1,
        ..TickReport::default()
    };
    let summary = tick_summary(&single, 20, chrono::Utc::now(), &roots(), None);
    assert!(summary.repo_limits.is_empty(), "no cap state was recorded");
}

/// A summary with no limits omits the key, and a payload without it (an
/// older daemon) decodes empty.
#[test]
fn an_old_summary_decodes_without_limits() {
    let wire = serde_json::to_value(crate::types::WorkFinderTickSummary::default()).unwrap();
    assert!(wire.get("repo_limits").is_none());
    let summary: crate::types::WorkFinderTickSummary = serde_json::from_value(wire).unwrap();
    assert!(summary.repo_limits.is_empty());
}
