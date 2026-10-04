//! A held PR's two views and its hold-aware series (#10284): which input
//! each heuristic reads, the replay of what twin-otter read, the refresh
//! cadence while held, and the seed of one hold visit.

use super::hold_parity::{fitted, serve, specs, AT};
use super::land_twin_otter::fixture_fit;
use super::{as_of, history_a, provenance};
use crate::eta::emit::{Trigger, HOURLY_CAP};
use crate::eta::explanation::Explanation;
use crate::eta::heuristics::{LandTwinOtter, LAND_TWIN_OTTER, LAND_TWIN_OTTER_B};
use crate::eta::queue_features::{reason, EventLog};
use crate::eta::simulate::run_explanation;
use crate::eta::tracker::{
    Emission, EstimateContext, ItemKey, ListedPr, PrState, PrView, Tracker, NOT_LISTED_YET,
};
use crate::eta::{EstimateInput, Heuristic, Kind, NoEstimateReason, Registry, StageSamples};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;
use std::sync::Arc;

const REPO: &str = "rjwalters/loom";
const RR: &str = "loom:review-requested";
const PR: &str = "loom:pr";
const OP: &str = "loom:operator";
const HOLD_AWARE: [&str; 2] = [LAND_TWIN_OTTER, LAND_TWIN_OTTER_B];

fn t(secs: i64) -> DateTime<Utc> {
    as_of() + Duration::seconds(secs)
}

fn key(number: u32) -> ItemKey {
    ItemKey::new(REPO, number + 1000)
}

/// The registry with the parity fixture's fit, cut off a day before
/// [`as_of`].
fn with_fit() -> Registry {
    Registry::with_fit(Some(Arc::new(fixture_fit(as_of() - Duration::days(1)))))
}

fn context<'a>(
    registry: &'a Registry,
    history: &'a StageSamples,
    repo_ids: &'a BTreeMap<String, u64>,
    refresh_secs: u64,
) -> EstimateContext<'a> {
    EstimateContext {
        registry,
        current_start: None,
        current_finish: None,
        current_land: None,
        history,
        refresh_secs,
        host_id: Some("host-test"),
        repo_ids,
    }
}

struct Harness {
    tracker: Tracker,
    history: StageSamples,
    repo_ids: BTreeMap<String, u64>,
}

impl Harness {
    fn new() -> Self {
        Harness {
            tracker: Tracker::new(provenance()),
            history: history_a(),
            repo_ids: BTreeMap::new(),
        }
    }

    /// One listing pass of `prs` (`(number, labels, updated_secs)`) at `at`.
    fn list(&mut self, prs: &[(u32, &[&str], i64)], at: i64) {
        let views: Vec<PrView> = prs
            .iter()
            .map(|(number, labels, updated)| PrView {
                number: *number,
                issue: number + 1000,
                labels: labels.iter().map(|s| (*s).to_string()).collect(),
                created_at: Some(t(-7200)),
                updated_at: Some(t(*updated)),
            })
            .collect();
        self.tracker.on_listing(REPO, &views, t(at), 300);
    }

    /// PR 501 first seen in review at 0, approved at 300, held at 600.
    fn held() -> Self {
        let mut h = Harness::new();
        h.list(&[(501, &[RR], -600)], 0);
        h.list(&[(501, &[PR], 250)], 300);
        h.list(&[(501, &[PR, OP], 550)], 600);
        h
    }

    /// The fleet view at `at`: PR 501 held, plus `others`.
    fn fleet(&mut self, others: &[(u32, &[&str], i64)], at: i64) {
        let mut listed = vec![ListedPr {
            number: 501,
            labels: vec![PR.to_string(), OP.to_string()],
            updated_at: Some(t(550)),
        }];
        listed.extend(others.iter().map(|(number, labels, updated)| ListedPr {
            number: *number,
            labels: labels.iter().map(|s| (*s).to_string()).collect(),
            updated_at: Some(t(*updated)),
        }));
        let listings = [(REPO.to_string(), listed)];
        self.tracker
            .on_fleet_context(&listings, EventLog::default(), t(at));
    }

    /// The `land` emissions of a full pass at `at`.
    fn land(&mut self, registry: &Registry, refresh_secs: u64, at: i64) -> Vec<Emission> {
        let ctx = context(registry, &self.history, &self.repo_ids, refresh_secs);
        self.tracker
            .estimate(None, &ctx, t(at))
            .into_iter()
            .filter(|e| e.explanation.kind == Kind::Land)
            .collect()
    }

    fn modeled(&self, registry: &Registry, at: i64) -> EstimateInput {
        let ctx = context(registry, &self.history, &self.repo_ids, 300);
        self.tracker
            .hold_aware_land_input(&key(501), &ctx, t(at))
            .expect("a land input")
    }
}

fn ids(emissions: &[Emission]) -> Vec<&str> {
    emissions
        .iter()
        .map(|e| e.explanation.heuristic.as_str())
        .collect()
}

fn twin(emissions: &[Emission]) -> &Explanation {
    &emissions
        .iter()
        .find(|e| e.explanation.heuristic == LAND_TWIN_OTTER)
        .expect("a twin-otter emission")
        .explanation
}

fn seed(e: &Explanation) -> String {
    e.combination.as_ref().expect("answered").seed.clone()
}

fn quantiles(e: &Explanation) -> (i64, i64, i64, i64) {
    let r = e.result.as_ref().expect("answered");
    (r.p25_sec, r.p50_sec, r.p75_sec, r.p90_sec.unwrap_or_default())
}

fn json(e: &Explanation) -> String {
    serde_json::to_string(e).unwrap()
}

// ---------------------------------------------------------- the two views

/// In the #10284 parity scenario every heuristic's estimate of a held PR is
/// exactly its estimate of its own view: the modeled one for the two
/// hold-aware ids, the described `blocked` one for the six path engines.
/// The modeled view's recorded features are the counts the model read, and
/// the answer replays from the JSON alone.
#[test]
fn each_heuristic_reads_its_own_view_of_a_held_pr() {
    let specs = specs();
    let mut tracker = serve(&specs);
    let registry = fitted();
    let history = history_a();
    let repo_ids = BTreeMap::new();
    let ctx = context(&registry, &history, &repo_ids, 300);
    let held = specs.iter().find(|s| s.pr == 32).unwrap().key();
    let at = super::fit_rows::h(AT);
    let described = tracker.land_input(&held, &ctx, at).unwrap();
    let modeled = tracker.hold_aware_land_input(&held, &ctx, at).unwrap();
    assert_eq!(modeled.current, described.current, "one stage, two feature sets");

    // Described: the `blocked` refusal's features, untouched.
    assert_eq!(described.features.ahead, None);
    assert!(described
        .features_omitted
        .iter()
        .any(|o| o.name == "ahead" && o.reason == reason::NO_STAGE));
    // Modeled: PR 31 is ahead by its hold entry (12 h), though its listing
    // was updated later (19 h) than PR 32's hold (15 h).
    assert_eq!(modeled.features.ahead, Some(1));
    assert_eq!(modeled.features.n_stage_repo, Some(1));
    assert_eq!(modeled.features.n_stage_fleet, Some(2));
    assert!(modeled.features_omitted.iter().all(|o| o.name != "ahead"));
    let mut same_otherwise = modeled.features.clone();
    same_otherwise.ahead = None;
    same_otherwise.n_stage_repo = None;
    same_otherwise.n_stage_fleet = None;
    for f in [
        &mut same_otherwise.exits_repo_1h,
        &mut same_otherwise.exits_repo_6h,
        &mut same_otherwise.exits_repo_24h,
        &mut same_otherwise.exits_fleet_1h,
        &mut same_otherwise.exits_fleet_6h,
        &mut same_otherwise.exits_fleet_24h,
    ] {
        *f = None;
    }
    assert_eq!(same_otherwise, described.features, "only the stage counts differ");

    let emissions = tracker.estimate(Some(&[held]), &ctx, at);
    let land: Vec<&Emission> = emissions
        .iter()
        .filter(|e| e.explanation.kind == Kind::Land)
        .collect();
    assert_eq!(land.len(), 6);
    for emission in land {
        let id = emission.explanation.heuristic.as_str();
        let heuristic = registry.get(id).unwrap();
        let view = if heuristic.models_hold() {
            &modeled
        } else {
            &described
        };
        assert_eq!(heuristic.models_hold(), HOLD_AWARE.contains(&id), "{id}");
        assert_eq!(json(&emission.explanation), json(&heuristic.estimate(view, &history)), "{id}");
        if !heuristic.models_hold() {
            assert_eq!(emission.explanation.no_estimate_reason, Some(NoEstimateReason::Blocked));
            continue;
        }
        let e = &emission.explanation;
        let record = e.twin_otter.as_ref().expect("answered");
        assert!(record.imputed.is_empty(), "{id}: {:?}", record.imputed);
        let recorded = e.features.as_ref().expect("features recorded");
        assert_eq!(record.input.ahead, recorded.ahead, "{id}: records what it read");
        assert_eq!(record.input.n_stage_fleet, recorded.n_stage_fleet, "{id}");
        let parsed: Explanation = serde_json::from_str(&json(e)).unwrap();
        assert_eq!(run_explanation(&parsed), e.quantiles_with_p90(), "{id}: replays");
    }
}

/// Without a fleet view the modeled counts stay omitted with their reason
/// and are imputed: nothing is fabricated.
#[test]
fn unavailable_context_stays_omitted_in_the_modeled_view() {
    let h = Harness::held();
    let modeled = h.modeled(&with_fit(), 700);
    assert_eq!(modeled.features.ahead, None);
    assert!(modeled
        .features_omitted
        .iter()
        .any(|o| o.name == "ahead" && o.reason == NOT_LISTED_YET));
    let e = LandTwinOtter::new(Some(Arc::new(fixture_fit(as_of() - Duration::days(1)))))
        .estimate(&modeled, &StageSamples::default());
    let imputed = &e.twin_otter.as_ref().expect("answered").imputed;
    assert!(imputed.iter().any(|f| f == "ahead"), "{imputed:?}");
}

// ---------------------------------------------------------- refresh

/// A held PR's hold-aware series transition at the hold's entry and its
/// release and refresh on the cadence (a tenth of slack) while held; the
/// path-engine series emit their `blocked` refusal once and stay silent.
#[test]
fn a_held_twin_otter_series_refreshes_while_the_path_engines_stay_silent() {
    let registry = with_fit();
    let mut h = Harness::new();
    h.list(&[(501, &[RR], -600)], 0);
    h.list(&[(501, &[PR], 250)], 300);
    assert_eq!(h.land(&registry, 300, 300).len(), 6);

    h.list(&[(501, &[PR, OP], 550)], 600);
    let entry = h.land(&registry, 300, 600);
    assert_eq!(entry.len(), 6);
    for e in &entry {
        let id = e.explanation.heuristic.as_str();
        assert_eq!(e.trigger, Trigger::Transition, "{id}");
        if HOLD_AWARE.contains(&id) {
            assert!(e.explanation.result.is_some(), "{id}");
        } else {
            assert_eq!(e.explanation.no_estimate_reason, Some(NoEstimateReason::Blocked), "{id}");
        }
    }

    assert!(h.land(&registry, 300, 869).is_empty(), "not yet due");
    let refresh = h.land(&registry, 300, 870);
    assert_eq!(ids(&refresh), HOLD_AWARE.to_vec());
    assert!(refresh.iter().all(|e| e.trigger == Trigger::Refresh));
    assert!(refresh.iter().all(|e| e.explanation.result.is_some()));
    assert!(h.land(&registry, 300, 1000).is_empty());
    assert_eq!(ids(&h.land(&registry, 300, 1170)), HOLD_AWARE.to_vec());

    // Released: every series transitions, the path engines answer again.
    h.list(&[(501, &[PR], 1450)], 1500);
    let released = h.land(&registry, 300, 1500);
    assert_eq!(released.len(), 6);
    assert!(released.iter().all(|e| e.trigger == Trigger::Transition));
    assert!(released
        .iter()
        .all(|e| e.explanation.no_estimate_reason != Some(NoEstimateReason::Blocked)));

    // Held again, then merged: no further estimate.
    h.list(&[(501, &[PR, OP], 1750)], 1800);
    assert_eq!(h.land(&registry, 300, 1800).len(), 6);
    h.list(&[], 2000);
    h.tracker
        .on_pr_resolved(&key(501), PrState::Merged(t(1950)), t(2000));
    assert!(h.land(&registry, 300, 2400).is_empty());
}

/// A twin-otter refusal while held (`no_model`) refreshes like any other
/// twin-otter refusal, so a fit that lands mid-hold is answered within one
/// interval.
#[test]
fn a_fit_that_lands_mid_hold_is_picked_up_within_one_interval() {
    let without = Registry::builtin();
    let mut h = Harness::held();
    let entry = h.land(&without, 300, 600);
    assert_eq!(twin(&entry).no_estimate_reason, Some(NoEstimateReason::NoModel));
    let refused = h.land(&without, 300, 870);
    assert_eq!(ids(&refused), HOLD_AWARE.to_vec());
    assert_eq!(twin(&refused).no_estimate_reason, Some(NoEstimateReason::NoModel));

    let answered = h.land(&with_fit(), 300, 1170);
    assert_eq!(ids(&answered), HOLD_AWARE.to_vec());
    assert!(answered.iter().all(|e| e.trigger == Trigger::Refresh));
    assert!(twin(&answered).result.is_some());
}

/// The hourly cap still bounds a held series, however short the cadence.
#[test]
fn the_hourly_cap_bounds_a_held_series() {
    let registry = with_fit();
    let mut h = Harness::held();
    let mut emitted: BTreeMap<String, usize> = BTreeMap::new();
    for k in 0..120 {
        for e in h.land(&registry, 1, 600 + 30 * k) {
            *emitted.entry(e.explanation.heuristic).or_default() += 1;
        }
    }
    for id in HOLD_AWARE {
        assert_eq!(emitted[id], HOURLY_CAP, "{id}");
    }
    assert_eq!(emitted["land-v1"], 1, "the refusal is never refreshed");
}

// ---------------------------------------------------------- the seed

/// One hold visit, one seed: two refreshes with identical features record
/// the same seed and quantiles; a changed queue context changes the
/// quantiles deterministically under the same seed; a release and re-entry
/// draws a new seed.
#[test]
fn one_hold_visit_keeps_its_seed_and_a_new_one_draws_another() {
    let registry = with_fit();
    let heuristic = LandTwinOtter::new(Some(Arc::new(fixture_fit(as_of() - Duration::days(1)))));
    let none = StageSamples::default();
    let mut h = Harness::held();
    h.fleet(&[], 650);
    let first = twin(&h.land(&registry, 300, 700)).clone();
    let refreshed = twin(&h.land(&registry, 300, 1000)).clone();
    assert_eq!(seed(&first), seed(&refreshed), "one visit, one seed");

    // Identical features at one instant: the same seed and quantiles.
    let alone = h.modeled(&registry, 1200);
    let (a, b) = (heuristic.estimate(&alone, &none), heuristic.estimate(&alone, &none));
    assert_eq!(json(&a), json(&b));
    assert_eq!(seed(&a), seed(&first));

    // Another held PR ahead (untracked: its `updated_at` bound), same
    // instant: new counts, new quantiles, the same seed.
    h.fleet(&[(502, &[PR, OP], 100)], 1100);
    let queued = h.modeled(&registry, 1200);
    assert_eq!((alone.features.ahead, queued.features.ahead), (Some(0), Some(1)));
    let c = heuristic.estimate(&queued, &none);
    assert_eq!(seed(&c), seed(&a));
    assert_ne!(quantiles(&c), quantiles(&a), "the queue context moved the estimate");
    assert_eq!(json(&c), json(&heuristic.estimate(&queued, &none)), "deterministically");

    // Released, then held again: a new visit, a new seed.
    h.list(&[(501, &[PR], 1450)], 1500);
    h.list(&[(501, &[PR, OP], 1750)], 1800);
    let again = twin(&h.land(&registry, 300, 1800)).clone();
    assert_ne!(seed(&again), seed(&first));
}
