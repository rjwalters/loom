//! Train/serve parity of "starred at `t`" (#10372).
//!
//! One scenario is built twice: as fleet snapshots plus raw forge events for
//! `rows::build_with_star` (training), and as tracker listings plus star
//! observations for `Tracker::on_star_context` (serving). At one row
//! instant, the training row's `star_source` / `starred_any` must equal what
//! the tracker records on the estimate's features, for an issue-only star, a
//! PR-only star, both, a star removed before `t`, and no star at all.

use super::fit_rows::{cutoff, h, row, REPO, STAR};
use super::history_a;
use super::hold_parity::{fitted, serve, snapshots, Spec, AT};
use crate::eta::fit::rows;
use crate::eta::fleet_events::{EventKind, ItemKind, RawEvent, SOURCE_FORGE};
use crate::eta::star::{RepoStar, StarInputs, StarSource};
use crate::eta::tracker::{EstimateContext, Tracker};
use crate::pr_latency::REVIEW_REQUESTED as RR;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

fn spec(pr: u32, steps: &'static [(f64, &'static [&'static str])]) -> Spec {
    Spec {
        repo: REPO,
        pr,
        steps,
        merged: None,
        touched: None,
    }
}

/// 51 issue-only, 52 PR-only, 53 both, 54 star removed before `t`, 55 none.
fn specs() -> Vec<Spec> {
    vec![
        spec(51, &[(10.0, &[RR])]),
        spec(52, &[(10.0, &[RR, STAR])]),
        spec(53, &[(10.0, &[RR, STAR])]),
        spec(54, &[(10.0, &[RR])]),
        spec(55, &[(10.0, &[RR])]),
        // A merge early on, so `since_merge` is known (rows need it).
        Spec {
            merged: Some(3.0),
            ..spec(59, &[(1.0, &[RR]), (2.0, &[crate::pr_latency::APPROVED])])
        },
    ]
}

fn raw(
    item: u32,
    kind_item: ItemKind,
    kind: EventKind,
    label: Option<&str>,
    at: DateTime<Utc>,
) -> RawEvent {
    RawEvent::new(REPO, item, kind_item, kind, label.map(str::to_string), at, SOURCE_FORGE, 1, at)
}

/// The raw cache: every PR links its issue from creation; issue 1051/1053
/// are starred at 12 h, 1054 at 12 h and unstarred at 15 h. With
/// `issue_stars` false, the star rows are left out (the self-check).
fn events(specs: &[Spec], issue_stars: bool) -> Vec<RawEvent> {
    let mut out = Vec::new();
    for s in specs {
        out.push(raw(s.issue(), ItemKind::Issue, EventKind::Opened, None, h(1.0)));
        out.push(
            raw(s.pr, ItemKind::Pr, EventKind::ClosingRef, Some("closes"), h(10.0))
                .with_target(Some(s.issue())),
        );
    }
    if issue_stars {
        for issue in [1051, 1053, 1054] {
            out.push(raw(issue, ItemKind::Issue, EventKind::LabelAdded, Some(STAR), h(12.0)));
        }
        out.push(raw(1054, ItemKind::Issue, EventKind::LabelRemoved, Some(STAR), h(15.0)));
    }
    out
}

fn trained(specs: &[Spec], issue_stars: bool) -> rows::Assembled {
    let mut inputs = StarInputs::default();
    inputs
        .repos
        .insert(REPO.to_string(), RepoStar::from_events(&events(specs, issue_stars)));
    rows::build_with_star(&snapshots(specs), cutoff(), Some(&inputs))
}

/// Serving: the hold-parity tracker, plus star observations at 10.5 h
/// (nothing starred yet), 12.5 h and 15.5 h.
fn served_tracker(specs: &[Spec]) -> Tracker {
    let mut tracker = serve(specs);
    let links: Vec<(u32, Vec<u32>)> = specs.iter().map(|s| (s.pr, vec![s.issue()])).collect();
    tracker.on_star_context(REPO, &links, Some(&[]), h(10.5));
    tracker.on_star_context(REPO, &links, Some(&[1051, 1053, 1054]), h(12.5));
    tracker.on_star_context(REPO, &links, Some(&[1051, 1053]), h(15.5));
    tracker.on_star_context(REPO, &links, Some(&[1051, 1053]), h(19.5));
    tracker
}

fn served(tracker: &mut Tracker, spec: &Spec) -> (Option<bool>, Option<String>) {
    let registry = fitted();
    let history = history_a();
    let repo_ids = BTreeMap::new();
    let ctx = EstimateContext {
        registry: &registry,
        current_start: None,
        current_finish: None,
        current_land: None,
        history: &history,
        refresh_secs: 300,
        host_id: Some("host-test"),
        repo_ids: &repo_ids,
    };
    let emissions = tracker.estimate(Some(&[spec.key()]), &ctx, h(AT));
    let features = emissions
        .iter()
        .find_map(|e| e.explanation.features.clone())
        .expect("an estimate with features");
    (features.starred_any, features.star_source)
}

fn expected() -> [(u32, StarSource); 5] {
    [
        (51, StarSource::Issue),
        (52, StarSource::Pr),
        (53, StarSource::Both),
        (54, StarSource::None),
        (55, StarSource::None),
    ]
}

#[test]
fn serving_records_the_star_source_training_recorded() {
    let specs = specs();
    let a = trained(&specs, true);
    let mut tracker = served_tracker(&specs);
    for (pr, want) in expected() {
        let spec = specs.iter().find(|s| s.pr == pr).unwrap();
        let training = row(&a, REPO, pr, h(AT)).unwrap_or_else(|| panic!("row {pr}"));
        assert_eq!(training.star_source, Some(want), "PR {pr} trained");
        assert_eq!(training.starred_any, Some(want.starred()), "PR {pr} trained");
        let (any, source) = served(&mut tracker, spec);
        assert_eq!(source.as_deref(), Some(want.as_str()), "PR {pr} served");
        assert_eq!(any, training.starred_any, "PR {pr}: serving differs from training");
        // The model input is untouched: PR-only.
        assert_eq!(training.inputs.starred, matches!(want, StarSource::Pr | StarSource::Both));
    }
}

/// Self-check: without the issue-star rows the training side disagrees with
/// serving on the issue-only PR, so the parity cannot pass vacuously.
#[test]
fn the_parity_fails_when_the_issue_star_rows_are_removed() {
    let specs = specs();
    let a = trained(&specs, false);
    let mut tracker = served_tracker(&specs);
    let spec = specs.iter().find(|s| s.pr == 51).unwrap();
    let training = row(&a, REPO, 51, h(AT)).unwrap();
    let (any, _) = served(&mut tracker, spec);
    assert_eq!(training.starred_any, Some(false));
    assert_ne!(any, training.starred_any);
}

#[test]
fn a_repo_without_cache_coverage_is_unknown_not_unstarred() {
    let specs = specs();
    let a = rows::build_with_star(&snapshots(&specs), cutoff(), Some(&StarInputs::default()));
    let training = row(&a, REPO, 55, h(AT)).unwrap();
    assert_eq!(training.starred_any, None);
    assert_eq!(training.star_source, None);
    assert!(a.stats.rows_star_unknown > 0);
}

#[test]
fn a_tracker_with_no_star_observation_records_nothing() {
    let specs = specs();
    let mut tracker = serve(&specs);
    let (any, source) = served(&mut tracker, &specs[0]);
    assert_eq!((any, source), (None, None));
}
