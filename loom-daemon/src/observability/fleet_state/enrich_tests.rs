//! Forge-dated entries, rework, conflict / CI flags, holds and level on
//! `fleet.state` rows (#11391).

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::*;
use crate::observability::fleet_state::history::HistoryEvent;
use crate::observability::fleet_state::outcomes::PullFacts;
use crate::observability::fleet_state::{
    build_view, hold_names, FleetInput, HeldSweep, ListedPr, ReadyItem, ReadyQueue, RepoListing,
};
use crate::telemetry::kinds::fleet_state::FleetStateRow;

const REPO: &str = "rjwalters/loom";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap()
}

#[derive(Default)]
struct Forge {
    histories: BTreeMap<u32, LabelHistory>,
    reads: Vec<u32>,
}

impl Forge {
    fn label(&mut self, n: u32, label: &str, at: DateTime<Utc>) {
        self.histories
            .entry(n)
            .or_default()
            .events
            .push((at, HistoryEvent::Labeled(label.to_string())));
    }
}

impl ForgeReads for Forge {
    fn pull(&mut self, _: &str, _: u32) -> Option<PullFacts> {
        None
    }
    fn issue(
        &mut self,
        _: &str,
        _: u32,
    ) -> Option<crate::observability::fleet_state::outcomes::IssueFacts> {
        Some(crate::observability::fleet_state::outcomes::IssueFacts::Open)
    }

    fn label_history(&mut self, _: &str, n: u32) -> Option<LabelHistory> {
        self.reads.push(n);
        self.histories.get(&n).cloned()
    }
}

fn labels(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

fn listing(prs: Vec<ListedPr>) -> RepoListing {
    RepoListing {
        repo: REPO.to_string(),
        prs,
    }
}

fn input(listings: Vec<RepoListing>) -> FleetInput {
    FleetInput {
        host_id: "h".to_string(),
        managed: [REPO.to_string()].into(),
        listings,
        listed_at: Some(t0()),
        ..FleetInput::default()
    }
}

fn run(
    inp: &FleetInput,
    prev: Option<&FleetView>,
    memory: &mut EnrichMemory,
    forge: &mut Forge,
    now: DateTime<Utc>,
) -> FleetView {
    let mut view = build_view(inp, prev, now);
    enrich(&mut view, memory, forge, &mut Histories::new(), now);
    view
}

fn row(view: &FleetView, issue: u32) -> &FleetStateRow {
    &view.repos[REPO].rows[&issue]
}

#[test]
fn a_restarted_row_is_dated_by_the_forge_and_no_longer_a_lower_bound() {
    let mut forge = Forge::default();
    let labeled_at = t0() - Duration::hours(5);
    forge.label(21, "loom:review-requested", labeled_at);
    let inp = input(vec![listing(vec![ListedPr {
        number: 21,
        labels: labels(&["loom:review-requested"]),
        issue: Some(20),
    }])]);
    let mut memory = EnrichMemory::default();
    let view = run(&inp, None, &mut memory, &mut forge, t0());
    assert!(!row(&view, 20).entered_at_lower_bound);
    assert_eq!(row(&view, 20).entered_at, labeled_at);

    // Carried to the next pass with no further read.
    let next = run(&inp, Some(&view), &mut memory, &mut forge, t0() + Duration::minutes(1));
    assert_eq!(row(&next, 20).entered_at, labeled_at);
    assert_eq!(forge.reads, vec![21]);
}

#[test]
fn a_ready_row_is_dated_by_its_issue_history() {
    let mut forge = Forge::default();
    let at = t0() - Duration::days(1);
    forge.label(7, "loom:issue", at);
    let mut inp = input(vec![]);
    inp.ready = Some(ReadyQueue {
        items: vec![ReadyItem {
            repo: REPO.to_string(),
            issue: 7,
            rank: 1,
            level: 0,
            weight: 100,
            created_at: None,
            main_red_fix: false,
            eligible_reason: None,
            eligible_at: None,
        }],
        ..ReadyQueue::default()
    });
    let view = run(&inp, None, &mut EnrichMemory::default(), &mut forge, t0());
    assert_eq!(row(&view, 7).entered_at, at);
    assert!(!row(&view, 7).entered_at_lower_bound);
}

#[test]
fn a_row_the_forge_cannot_date_stays_a_lower_bound_and_is_not_reread() {
    let mut forge = Forge::default();
    forge.label(21, "loom:pr", t0() - Duration::hours(1)); // not its stage label
    let inp = input(vec![listing(vec![ListedPr {
        number: 21,
        labels: labels(&["loom:review-requested"]),
        issue: Some(20),
    }])]);
    let mut memory = EnrichMemory::default();
    let view = run(&inp, None, &mut memory, &mut forge, t0());
    assert!(row(&view, 20).entered_at_lower_bound);
    run(&inp, Some(&view), &mut memory, &mut forge, t0() + Duration::minutes(1));
    assert_eq!(forge.reads, vec![21], "one read: undatable is remembered");
}

#[test]
fn a_sweep_row_with_a_lower_bound_entry_is_dated_again_each_pass_without_a_read() {
    let mut forge = Forge::default();
    let at = t0() - Duration::hours(2);
    forge.label(21, "loom:review-requested", at);
    let mut inp = input(vec![]);
    inp.held = vec![HeldSweep {
        repo: REPO.to_string(),
        issue: 20,
        stage: FleetStage::ReviewWait,
        entered_at: t0() - Duration::hours(9),
        entered_at_lower_bound: true,
        pr: Some(21),
        overflow: false,
        start: Default::default(),
        sweep_id: None,
    }];
    let mut memory = EnrichMemory::default();
    let first = run(&inp, None, &mut memory, &mut forge, t0());
    assert_eq!(row(&first, 20).entered_at, at);
    let second = run(&inp, Some(&first), &mut memory, &mut forge, t0() + Duration::minutes(1));
    assert_eq!(row(&second, 20).entered_at, at);
    assert!(!row(&second, 20).entered_at_lower_bound);
    assert_eq!(forge.reads, vec![21]);
}

#[test]
fn reads_per_pass_are_capped() {
    let mut forge = Forge::default();
    let prs = (1..=150)
        .map(|n| ListedPr {
            number: 1000 + n,
            labels: labels(&["loom:review-requested"]),
            issue: Some(n),
        })
        .collect();
    let view =
        run(&input(vec![listing(prs)]), None, &mut EnrichMemory::default(), &mut forge, t0());
    assert_eq!(forge.reads.len(), MAX_READS_PER_PASS);
    assert_eq!(view.repos[REPO].rows.len(), 150);
}

#[test]
fn rework_counts_judge_rejections_and_ignores_a_relabel_inside_one_attempt() {
    let mut forge = Forge::default();
    for (label, h) in [
        ("loom:review-requested", 10),
        ("loom:changes-requested", 9),
        ("loom:changes-requested", 8), // relabel, same attempt
        ("loom:review-requested", 7),
        ("loom:changes-requested", 6),
        ("loom:review-requested", 5),
        ("loom:pr", 4),
    ] {
        forge.label(21, label, t0() - Duration::hours(h));
    }
    let history = forge.histories[&21].clone();
    assert_eq!(rejections(&history, t0()), 2);
    assert_eq!(rejections(&history, t0() - Duration::hours(8)), 1);

    let inp = input(vec![listing(vec![ListedPr {
        number: 21,
        labels: labels(&["loom:pr"]),
        issue: Some(20),
    }])]);
    let view = run(&inp, None, &mut EnrichMemory::default(), &mut forge, t0());
    assert_eq!(row(&view, 20).rework, 2);
}

#[test]
fn rework_grows_by_one_when_a_row_enters_doctor() {
    let mut forge = Forge::default();
    let review = input(vec![listing(vec![ListedPr {
        number: 21,
        labels: labels(&["loom:review-requested"]),
        issue: Some(20),
    }])]);
    let mut memory = EnrichMemory::default();
    let first = run(&review, None, &mut memory, &mut forge, t0());
    assert_eq!(row(&first, 20).rework, 0);
    let doctor = input(vec![listing(vec![ListedPr {
        number: 21,
        labels: labels(&["loom:changes-requested"]),
        issue: Some(20),
    }])]);
    let second = run(&doctor, Some(&first), &mut memory, &mut forge, t0() + Duration::minutes(1));
    assert_eq!(row(&second, 20).rework, 1);
    let third = run(&doctor, Some(&second), &mut memory, &mut forge, t0() + Duration::minutes(2));
    assert_eq!(row(&third, 20).rework, 1);
}

#[test]
fn pr_rows_carry_conflict_ci_fail_holds_star_and_level() {
    let inp = input(vec![listing(vec![
        ListedPr {
            number: 21,
            labels: labels(&[
                "loom:pr",
                "loom:merge-conflict",
                "loom:ci-failure",
                "loom:operator",
                "loom:sequenced",
                "loom:very-important",
            ]),
            issue: Some(20),
        },
        ListedPr {
            number: 22,
            labels: labels(&["loom:review-requested"]),
            issue: Some(30),
        },
    ])]);
    let view = build_view(&inp, None, t0());
    let r = row(&view, 20);
    assert!(r.conflict && r.ci_fail);
    assert_eq!(r.holds, vec!["operator".to_string(), "sequenced".to_string()]);
    assert_eq!(r.level, 2, "the PR's own level label (#11510)");
    let plain = row(&view, 30);
    assert!(!plain.conflict && !plain.ci_fail);
    assert!(plain.holds.is_empty());
    assert_eq!(plain.level, 0);
}

#[test]
fn a_sweep_row_takes_the_linked_prs_flags_and_keeps_its_earlier_level() {
    let mut inp = input(vec![listing(vec![ListedPr {
        number: 21,
        labels: labels(&[
            "loom:review-requested",
            "loom:merge-conflict",
            "loom:blocked",
        ]),
        issue: Some(20),
    }])]);
    inp.held = vec![HeldSweep {
        repo: REPO.to_string(),
        issue: 20,
        stage: FleetStage::ReviewWait,
        entered_at: t0(),
        entered_at_lower_bound: false,
        pr: Some(21),
        overflow: false,
        start: Default::default(),
        sweep_id: None,
    }];
    let mut prev = build_view(&input(vec![]), None, t0());
    let mut earlier = FleetStateRow::new(20, FleetStage::ReadyWait, t0());
    earlier.level = 2;
    let state = prev.repos.entry(REPO.to_string()).or_default();
    state.rows.insert(20, earlier);
    state
        .sources
        .insert(20, crate::observability::fleet_state::Source::Ready);
    let view = build_view(&inp, Some(&prev), t0() + Duration::minutes(1));
    let r = row(&view, 20);
    assert!(r.conflict);
    assert_eq!(r.holds, vec!["blocked".to_string()]);
    assert_eq!(r.level, 2);
}

#[test]
fn hold_names_lists_every_hold_strongest_first() {
    assert!(hold_names(&labels(&["loom:pr"])).is_empty());
    assert_eq!(
        hold_names(&labels(&["loom:blocked", "loom:operator-decision", "loom:operator"])),
        vec!["operator_decision", "operator", "blocked"]
    );
}

#[test]
fn the_new_fields_round_trip_and_are_absent_when_empty() {
    let bare = serde_json::to_value(FleetStateRow::new(1, FleetStage::ReviewWait, t0())).unwrap();
    for key in ["rework", "conflict", "ci_fail", "holds", "level"] {
        assert!(bare.get(key).is_none(), "{key} must be absent when empty");
    }
    let mut full = FleetStateRow::new(1, FleetStage::Doctor, t0());
    full.rework = 2;
    full.conflict = true;
    full.ci_fail = true;
    full.holds = vec!["blocked".to_string()];
    full.level = 2;
    let back: FleetStateRow = serde_json::from_value(serde_json::to_value(&full).unwrap()).unwrap();
    assert_eq!(back, full);
    // A v1 row without any of them still reads.
    let old: FleetStateRow = serde_json::from_value(bare).unwrap();
    assert_eq!(old.rework, 0);
}
