//! Train/serve parity of a held PR's twin-otter input (#10284).
//!
//! One fleet scenario is built twice: as fleet snapshots for
//! `fit::rows::build` (training), and as tracker listings, journal rows and
//! a fleet context (serving). At one row instant, the input the tracker
//! hands `land-2026-10-04-twin-otter` for a held PR must equal that PR's
//! `merge_hold` training row, with nothing imputed.
//!
//! Every API this file uses predates #10284, so it runs unchanged against
//! the code before the fix, where it fails: a held PR's stage-dependent
//! counts were `no_stage` and imputed.

use super::fit_rows::{cutoff, h, row, secs, snapshot, OPERATOR, OTHER, REPO};
use super::history_a;
use super::land_twin_otter::fixture_fit;
use super::provenance;
use crate::eta::fit::rows;
use crate::eta::fit::{FitStage, ModelInputs};
use crate::eta::fleet::FleetSnapshot;
use crate::eta::heuristics::LAND_TWIN_OTTER;
use crate::eta::journal::JournalEntry;
use crate::eta::tracker::{
    events_from_journal, EstimateContext, ItemKey, ListedPr, PrState, PrView, Tracker,
};
use crate::eta::twin_otter::TwinOtterInput;
use crate::eta::{Kind, Registry};
use crate::pr_latency::history::fixtures::{labeled, t, unlabeled};
use crate::pr_latency::history::{PrEvent, PrHistory, PrState as HistoryState};
use crate::pr_latency::{APPROVED, REVIEW_REQUESTED};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;
use std::sync::Arc;

const RR: &str = REVIEW_REQUESTED;
const PR: &str = APPROVED;
const OP: &str = OPERATOR;

/// One PR of the scenario: its label sets over time (hours after the
/// fixture epoch), its merge, and its last non-label activity.
pub(crate) struct Spec {
    pub(crate) repo: &'static str,
    pub(crate) pr: u32,
    pub(crate) steps: &'static [(f64, &'static [&'static str])],
    pub(crate) merged: Option<f64>,
    /// A later push or comment: moves the listing's `updated_at` only.
    pub(crate) touched: Option<f64>,
}

impl Spec {
    pub(crate) fn key(&self) -> ItemKey {
        ItemKey::new(self.repo, self.issue())
    }

    pub(crate) fn issue(&self) -> u32 {
        self.pr + 1000
    }

    fn listed_at(&self, at: f64) -> bool {
        self.steps[0].0 <= at && self.merged.is_none_or(|m| m > at)
    }

    /// The labels at `at`, and when they (or anything else) last changed.
    fn view_at(&self, at: f64) -> (Vec<String>, DateTime<Utc>) {
        let (since, labels) = self
            .steps
            .iter()
            .rev()
            .find(|(x, _)| *x <= at)
            .expect("listed");
        let touched = self.touched.filter(|x| *x <= at).unwrap_or(*since);
        let labels = labels.iter().map(|s| (*s).to_string()).collect();
        (labels, h(since.max(touched)))
    }

    /// The forge's label timeline of this PR.
    pub(crate) fn history(&self) -> PrHistory {
        let mut events: Vec<PrEvent> = Vec::new();
        let mut before: &[&str] = &[];
        for (x, labels) in self.steps {
            let at = secs(h(*x));
            for gone in before.iter().filter(|l| !labels.contains(l)) {
                events.push(unlabeled(gone, at));
            }
            for new in labels.iter().filter(|l| !before.contains(l)) {
                events.push(labeled(new, at));
            }
            before = labels;
        }
        let (state, merged_at) = match self.merged {
            Some(m) => {
                events.push(PrEvent::Merged { at: h(m) });
                (HistoryState::Merged, Some(h(m)))
            }
            None => (HistoryState::Open, None),
        };
        PrHistory::new(self.pr, t(0), state, merged_at, Vec::new(), events, true)
    }
}

/// The scenario. [`REPO`]:
///
/// - 31: review 10 h, approved 11 h, **held from 12 h** (open); touched at
///   19 h, so its listing's `updated_at` is later than PR 32's;
/// - 32: review 13 h, approved 14 h, **held from 15 h** (open);
/// - 33: review 9 h, approved 10 h, held 16 h, **merged while held** 17 h;
/// - 34: review 14.5 h, approved 16 h, held 16.5 h, **released** 18 h;
/// - 35: review 15 h, approved 17 h: `merge_wait`.
///
/// [`OTHER`]: 41 review 12 h, approved 13 h, held from 14 h (open); 42
/// review 15 h, approved 16 h, merged 18.5 h.
pub(crate) fn specs() -> Vec<Spec> {
    let spec = |repo, pr, steps, merged, touched| Spec {
        repo,
        pr,
        steps,
        merged,
        touched,
    };
    vec![
        spec(REPO, 31, &[(10.0, &[RR]), (11.0, &[PR]), (12.0, &[PR, OP])], None, Some(19.0)),
        spec(REPO, 32, &[(13.0, &[RR]), (14.0, &[PR]), (15.0, &[PR, OP])], None, None),
        spec(REPO, 33, &[(9.0, &[RR]), (10.0, &[PR]), (16.0, &[PR, OP])], Some(17.0), None),
        spec(
            REPO,
            34,
            &[
                (14.5, &[RR]),
                (16.0, &[PR]),
                (16.5, &[PR, OP]),
                (18.0, &[PR]),
            ],
            None,
            None,
        ),
        spec(REPO, 35, &[(15.0, &[RR]), (17.0, &[PR])], None, None),
        spec(OTHER, 41, &[(12.0, &[RR]), (13.0, &[PR]), (14.0, &[PR, OP])], None, None),
        spec(OTHER, 42, &[(15.0, &[RR]), (16.0, &[PR])], Some(18.5), None),
    ]
}

/// The last listing pass, and the instant the parity is checked at: a row
/// instant (`ROW_STEP_SEC` grid from `t(0)`), with every fact at least 1.5 h
/// old.
pub(crate) const LAST_PASS: f64 = 19.5;
pub(crate) const AT: f64 = 20.0;

/// The training side: one snapshot per repo.
pub(crate) fn snapshots(specs: &[Spec]) -> Vec<FleetSnapshot> {
    [REPO, OTHER]
        .into_iter()
        .map(|repo| {
            let prs: Vec<PrHistory> = specs
                .iter()
                .filter(|s| s.repo == repo)
                .map(Spec::history)
                .collect();
            snapshot(repo, &prs, cutoff() + Duration::hours(1))
        })
        .collect()
}

/// The fleet listings of `specs` at `at`.
pub(crate) fn listings_at(specs: &[Spec], at: f64) -> Vec<(String, Vec<ListedPr>)> {
    [REPO, OTHER]
        .into_iter()
        .map(|repo| {
            let prs = specs
                .iter()
                .filter(|s| s.repo == repo && s.listed_at(at))
                .map(|s| {
                    let (labels, updated_at) = s.view_at(at);
                    ListedPr {
                        number: s.pr,
                        labels,
                        updated_at: Some(updated_at),
                    }
                })
                .collect();
            (repo.to_string(), prs)
        })
        .collect()
}

/// The tracker's listing rows of `repo`'s PRs at `at`.
pub(crate) fn views_at(specs: &[Spec], repo: &str, at: f64) -> Vec<PrView> {
    specs
        .iter()
        .filter(|s| s.repo == repo && s.listed_at(at))
        .map(|s| {
            let (labels, updated_at) = s.view_at(at);
            PrView {
                number: s.pr,
                issue: s.issue(),
                labels,
                created_at: Some(t(0)),
                updated_at: Some(updated_at),
            }
        })
        .collect()
}

/// The serving side: a tracker driven through every listing pass of
/// `specs` up to [`LAST_PASS`], each with its fleet context, as the daemon's
/// ETA pass does (the last pass's view is the one an estimate reads).
pub(crate) fn serve(specs: &[Spec]) -> Tracker {
    serve_with(specs, &[], |_, _| {})
}

/// [`serve`] with extra pass instants `extra`, calling `pass(tracker, at)`
/// on every pass after its listings and before its fleet context.
pub(crate) fn serve_with(
    specs: &[Spec],
    extra: &[f64],
    mut pass: impl FnMut(&mut Tracker, f64),
) -> Tracker {
    let mut instants: Vec<f64> = specs
        .iter()
        .flat_map(|s| {
            s.steps
                .iter()
                .map(|(x, _)| *x)
                .chain(s.merged)
                .chain(s.touched)
        })
        .chain([LAST_PASS])
        .chain(extra.iter().copied())
        .collect();
    instants.sort_by(f64::total_cmp);
    instants.dedup();
    let mut tracker = Tracker::new(provenance());
    let mut journal: Vec<JournalEntry> = Vec::new();
    for at in instants {
        for repo in [REPO, OTHER] {
            let listed: Vec<PrView> = specs
                .iter()
                .filter(|s| s.repo == repo && s.listed_at(at))
                .map(|s| {
                    let (labels, updated_at) = s.view_at(at);
                    PrView {
                        number: s.pr,
                        issue: s.issue(),
                        labels,
                        created_at: Some(t(0)),
                        updated_at: Some(updated_at),
                    }
                })
                .collect();
            journal.extend(tracker.on_listing(repo, &listed, h(at), 300).journal);
            for s in specs
                .iter()
                .filter(|s| s.repo == repo && s.merged == Some(at))
            {
                let effects = tracker.on_pr_resolved(&s.key(), PrState::Merged(h(at)), h(at));
                journal.extend(effects.journal);
            }
        }
        pass(&mut tracker, at);
        let events = events_from_journal(&journal, h(at));
        tracker.on_fleet_context(&listings_at(specs, at), events, h(at));
    }
    tracker
}

/// The registry with the parity fixture's fit, cut off before the scenario.
pub(crate) fn fitted() -> Registry {
    let at = t(0) - Duration::days(1);
    // Every fit, so `land-2026-10-06-keen-wren` (#10508) and
    // `land-2026-10-06-loop-kite` (#10521) answer too.
    Registry::with_all_fits(
        Some(Arc::new(fixture_fit(at))),
        Some(Arc::new(super::keen_wren::v2_fixture(at, 0.0, 0.0))),
        Some(Arc::new(super::loop_kite::v3_fixture(at, 0.0, 0.0))),
    )
}

/// The twin-otter record of `spec`'s first `land` estimate at [`AT`].
pub(crate) fn served(
    tracker: &mut Tracker,
    registry: &Registry,
    spec: &Spec,
) -> (TwinOtterInput, Vec<String>) {
    let history = history_a();
    let repo_ids = BTreeMap::new();
    let ctx = EstimateContext {
        registry,
        current_start: None,
        current_finish: None,
        current_land: None,
        history: &history,
        refresh_secs: 300,
        host_id: Some("host-test"),
        repo_ids: &repo_ids,
        stalls: &super::NO_STALLS,
    };
    let emissions = tracker.estimate(Some(&[spec.key()]), &ctx, h(AT));
    let twin = emissions
        .iter()
        .find(|e| e.explanation.kind == Kind::Land && e.explanation.heuristic == LAND_TWIN_OTTER)
        .expect("a twin-otter estimate");
    let record = twin.explanation.twin_otter.as_ref().unwrap_or_else(|| {
        panic!("PR {} not answered: {:?}", spec.pr, twin.explanation.no_estimate_reason)
    });
    (record.input.clone(), record.imputed.clone())
}

/// The fields the parity covers: the nine queue counts, then the age, the
/// rework count and the operator-hold flag.
fn served_fields(input: &TwinOtterInput) -> (Vec<Option<f64>>, (f64, u32, u8)) {
    let counts = [
        input.ahead,
        input.n_stage_repo,
        input.n_stage_fleet,
        input.exits_repo_6h,
        input.exits_repo_24h,
        input.exits_fleet_6h,
        input.merges_repo_24h,
        input.merges_fleet_6h,
    ];
    let mut out: Vec<Option<f64>> = counts.iter().map(|c| c.map(f64::from)).collect();
    out.push(input.since_merge_h);
    (out, (input.age_h, input.rework, input.op_hold))
}

fn trained_fields(inputs: &ModelInputs) -> (Vec<Option<f64>>, (f64, u32, u8)) {
    let counts = [
        inputs.ahead,
        inputs.n_stage_repo,
        inputs.n_stage_fleet,
        inputs.exits_repo_6h,
        inputs.exits_repo_24h,
        inputs.exits_fleet_6h,
        inputs.merges_repo_24h,
        inputs.merges_fleet_6h,
    ];
    let mut out: Vec<Option<f64>> = counts.iter().map(|c| Some(f64::from(*c))).collect();
    out.push(Some(inputs.since_merge_h));
    (out, (inputs.age_h, inputs.rework, u8::from(inputs.op_hold)))
}

#[test]
fn a_held_prs_twin_otter_input_is_its_merge_hold_training_row() {
    let specs = specs();
    let trained = rows::build(&snapshots(&specs), cutoff());
    let mut tracker = serve(&specs);
    let registry = fitted();

    // By hand, at 20 h. `merge_hold` in REPO: 31 (12 h), 32 (15 h); OTHER:
    // 41. Its departures in the last 6 h: 33's merge (17 h) and 34's release
    // (18 h). Merges: 33 (17 h) in REPO, 42 (18.5 h) in OTHER.
    let by_hand = |ahead: f64, age_h: f64| {
        let counts = [ahead, 1.0, 2.0, 2.0, 2.0, 2.0, 1.0, 2.0, 3.0];
        (counts.map(Some).to_vec(), (age_h, 0, 1))
    };
    for (pr, ahead, age_h) in [(31, 0.0, 8.0), (32, 1.0, 5.0)] {
        let spec = specs.iter().find(|s| s.pr == pr).unwrap();
        let training = row(&trained, REPO, pr, h(AT)).unwrap_or_else(|| panic!("row {pr}"));
        assert_eq!(training.stage, FitStage::MergeHold, "PR {pr}");
        assert_eq!(trained_fields(&training.inputs), by_hand(ahead, age_h), "PR {pr} trained");

        let (input, imputed) = served(&mut tracker, &registry, spec);
        assert_eq!(input.stage, "merge_hold", "PR {pr}");
        assert_eq!(
            served_fields(&input),
            trained_fields(&training.inputs),
            "PR {pr}: serving differs from training"
        );
        assert!(imputed.is_empty(), "PR {pr} imputed {imputed:?}");
    }
}

/// A released PR, and a never-held peer approved before its release (#10312):
/// serving's twin-otter input equals training's `merge_wait` row.
///
/// At 20 h, `merge_wait` in [`REPO`] is 34 (episode entered at its 18 h
/// release, approval 16 h) and 35 (17 h). Training sorts by the episode, so
/// 35 is ahead of 34: `ahead` is 1 for 34 and 0 for 35.
#[test]
fn a_released_prs_and_its_never_held_peers_input_is_their_merge_wait_training_row() {
    let specs = specs();
    let trained = rows::build(&snapshots(&specs), cutoff());
    let mut tracker = serve(&specs);
    let registry = fitted();
    for (pr, ahead, age_h) in [(34, 1.0, 2.0), (35, 0.0, 3.0)] {
        let spec = specs.iter().find(|s| s.pr == pr).unwrap();
        let training = row(&trained, REPO, pr, h(AT)).unwrap_or_else(|| panic!("row {pr}"));
        assert_eq!(training.stage, FitStage::MergeWait, "PR {pr}");
        let (counts, rest) = trained_fields(&training.inputs);
        assert_eq!(counts[0], Some(ahead), "PR {pr} trained ahead");
        assert_eq!(rest, (age_h, 0, 0), "PR {pr} trained");

        let (input, imputed) = served(&mut tracker, &registry, spec);
        assert_eq!(input.stage, "merge_wait", "PR {pr}");
        assert_eq!(
            served_fields(&input),
            trained_fields(&training.inputs),
            "PR {pr}: serving differs from training"
        );
        assert!(imputed.is_empty(), "PR {pr} imputed {imputed:?}");
    }
}
