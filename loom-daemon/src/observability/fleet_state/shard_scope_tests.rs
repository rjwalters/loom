use super::*;

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, TimeZone, Utc};

use crate::observability::fleet_state::outcomes::{
    self, ForgeReads, IssueFacts, LabelHistory, Memory, Pass, PullFacts,
};
use crate::observability::fleet_state::{build_view, FleetInput, ListedPr, RepoListing, RepoView};
use crate::telemetry::kinds::fleet_state::{FleetStage, FleetStateRow};
use crate::telemetry::provenance::Provenance;

const REPO: &str = "acme/b";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap()
}

fn view() -> FleetView {
    let mut repo = RepoView::default();
    for (issue, source, stage) in [
        (1, Source::Review, FleetStage::ReviewWait),
        (2, Source::Held, FleetStage::SweepBuilder),
        (3, Source::Ready, FleetStage::ReadyWait),
    ] {
        repo.rows
            .insert(issue, FleetStateRow::new(issue, stage, t0()));
        repo.sources.insert(issue, source);
    }
    let mut view = FleetView::default();
    view.repos.insert("acme/a".to_string(), repo.clone());
    view.repos.insert(REPO.to_string(), repo);
    view.observed
        .insert(Source::Review, ["acme/a", REPO].iter().map(|s| (*s).to_string()).collect());
    view
}

fn unowned() -> BTreeSet<String> {
    [REPO.to_string()].into_iter().collect()
}

#[test]
fn split_keeps_owned_roots_and_names_the_rest() {
    let roots = vec![
        (PathBuf::from("/w/a"), "acme/a".to_string()),
        (PathBuf::from("/w/b"), REPO.to_string()),
    ];
    let (listed, unowned) = split_with(&roots, |slug| slug == "acme/a");
    assert_eq!(listed, vec![(PathBuf::from("/w/a"), "acme/a".to_string())]);
    assert_eq!(unowned, self::unowned());
}

#[test]
fn forget_drops_only_review_rows_of_unowned_repos() {
    let mut v = view();
    forget_view(&mut v, &unowned());
    assert_eq!(v.repos["acme/a"].rows.len(), 3, "an owned repo is untouched");
    let b = &v.repos[REPO];
    assert_eq!(b.rows.keys().copied().collect::<Vec<_>>(), vec![2, 3]);
    assert!(!b.sources.values().any(|s| *s == Source::Review));
    assert!(!v.observed[&Source::Review].contains(REPO));
    assert!(v.observed[&Source::Review].contains("acme/a"));
}

/// A forge that counts reads and answers nothing.
#[derive(Default)]
struct Counting {
    reads: usize,
}

impl ForgeReads for Counting {
    fn pull(&mut self, _: &str, _: u32) -> Option<PullFacts> {
        self.reads += 1;
        None
    }
    fn label_history(&mut self, _: &str, _: u32) -> Option<LabelHistory> {
        self.reads += 1;
        None
    }
    fn issue(&mut self, _: &str, _: u32) -> Option<IssueFacts> {
        self.reads += 1;
        Some(IssueFacts::Open)
    }
}

fn input(listed: bool) -> FleetInput {
    FleetInput {
        champion: BTreeMap::new(),
        curator: BTreeMap::new(),
        host_id: "host-a".to_string(),
        managed: [REPO.to_string()].into(),
        held: Vec::new(),
        listings: if listed {
            vec![RepoListing {
                repo: REPO.to_string(),
                prs: vec![ListedPr {
                    number: 21,
                    labels: vec!["loom:review-requested".to_string()],
                    issue: Some(20),
                }],
            }]
        } else {
            Vec::new()
        },
        listed_at: Some(t0()),
        ready: None,
        slots: None,
        capacity: None,
        main_ci: BTreeMap::new(),
    }
}

/// One collector pass: optionally forget, then build, diff and remember.
fn pass(
    input: &FleetInput,
    prev: &mut Option<FleetView>,
    memory: &mut Memory,
    forge: &mut Counting,
    forget_unowned: bool,
    now: DateTime<Utc>,
) -> (FleetView, usize) {
    if forget_unowned {
        forget_parts(prev.as_mut(), memory, &unowned());
    }
    let view = build_view(input, prev.as_ref(), now);
    let listed = outcomes::listed(&input.listings);
    let p = Pass {
        view: &view,
        listed: &listed,
        managed: &input.managed,
        now,
    };
    let provenance = Provenance {
        version: "0.0.0".to_string(),
        revision: "0".repeat(40),
        tree_state: "clean".to_string(),
        complete: true,
    };
    let records = outcomes::diff(memory, &p, forge, &provenance);
    *memory = outcomes::remember(std::mem::take(memory), &p);
    *prev = Some(view.clone());
    (view, records.len())
}

fn has_row(view: &FleetView, issue: u32) -> bool {
    view.repos
        .get(REPO)
        .is_some_and(|r| r.rows.contains_key(&issue))
}

#[test]
fn a_handover_drops_the_rows_without_an_outcome_or_a_forge_read() {
    let (mut prev, mut memory, mut forge) = (None, Memory::default(), Counting::default());
    let step = Duration::minutes(5);
    let (v1, _) = pass(&input(true), &mut prev, &mut memory, &mut forge, false, t0());
    assert!(has_row(&v1, 20), "owned and listed: the PR row is there");

    // The repo moved to another host: not listed, and forgotten.
    let (v2, n2) = pass(&input(false), &mut prev, &mut memory, &mut forge, true, t0() + step);
    assert!(!has_row(&v2, 20), "an unowned repo's review row is gone, not carried forward");
    assert_eq!(n2, 0, "stopping to observe is no stage exit");
    assert_eq!(forge.reads, 0, "and costs no forge read");

    // Back to this host: first seen mid-stage, still no outcome.
    let (v3, n3) = pass(&input(true), &mut prev, &mut memory, &mut forge, false, t0() + step * 2);
    let row = &v3.repos[REPO].rows[&20];
    assert!(row.entered_at_lower_bound, "a re-owned row's entry is a lower bound");
    assert_eq!(n3, 0);
}

#[test]
fn without_forget_an_unlisted_repo_would_keep_its_stale_rows() {
    // The control for the test above: the plain failed-listing path carries
    // the rows forward forever, which is why a handover must forget them.
    let (mut prev, mut memory, mut forge) = (None, Memory::default(), Counting::default());
    pass(&input(true), &mut prev, &mut memory, &mut forge, false, t0());
    let (v2, _) = pass(
        &input(false),
        &mut prev,
        &mut memory,
        &mut forge,
        false,
        t0() + Duration::minutes(5),
    );
    assert!(has_row(&v2, 20));
}
