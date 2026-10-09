//! Walk-forward coefficient files for a replay (#10524): each estimate is
//! served by the newest file strictly before its own `as_of`, the unfitted
//! registry before every file, and no file at all is the builtin registry.

use super::history_a;
use super::land_twin_otter::{fit_as_of, fixture_fit, review_input};
use crate::eta::fit::{coeffs, CoefficientFile};
use crate::eta::heuristics::{LAND_BRISK_PETREL, LAND_TWIN_OTTER_B, LAND_V2};
use crate::eta::walk_forward::DatedFits;
use crate::eta::{Heuristic, NoEstimateReason, Registry, StageSamples};
use chrono::{DateTime, Duration, Utc};
use std::sync::Arc;

fn bytes(e: &crate::eta::Explanation) -> String {
    serde_json::to_string(e).unwrap()
}

/// [`fixture_fit`] at `as_of`, made distinguishable by `tag`.
fn tagged(as_of: DateTime<Utc>, tag: &str) -> CoefficientFile {
    let mut file = fixture_fit(as_of);
    file.fitter.version = tag.to_string();
    file.with_derived_id()
}

#[test]
fn no_file_is_the_builtin_registry_byte_for_byte() {
    let fits = DatedFits::new(Vec::new());
    assert!(fits.is_empty());
    let input = review_input();
    let history = history_a();
    let builtin = Registry::builtin();
    assert_eq!(fits.registry().ids(), builtin.ids());
    for id in builtin.ids() {
        let walked = fits.heuristic(id).expect("registered");
        let direct = builtin.get(id).unwrap();
        assert_eq!(walked.id(), direct.id());
        assert_eq!(walked.kind(), direct.kind());
        assert_eq!(walked.tier(), direct.tier());
        assert_eq!(walked.models_hold(), direct.models_hold());
        assert_eq!(
            bytes(&walked.estimate(&input, &history)),
            bytes(&direct.estimate(&input, &history)),
            "{id}"
        );
    }
    let refused = fits
        .heuristic(LAND_TWIN_OTTER_B)
        .unwrap()
        .estimate(&input, &StageSamples::default());
    assert_eq!(refused.no_estimate_reason, Some(NoEstimateReason::NoModel));
    assert!(fits.heuristic("land-nope").is_none());
}

#[test]
fn each_estimate_is_served_by_the_newest_file_strictly_before_it() {
    let first = fit_as_of();
    let second = first + Duration::days(1);
    let a = tagged(first, "a");
    let b = tagged(second, "b");
    let (a_id, b_id) = (a.id.clone(), b.id.clone());
    // Order of the input does not matter.
    let fits = DatedFits::new(vec![b, a]);
    assert_eq!(fits.len(), 2);
    assert_eq!(fits.cutoffs(), vec![first, second]);
    // At or before the first cutoff: no file is usable (strictly before).
    assert_eq!(fits.at(first - Duration::hours(1)).fit_id(), None);
    assert_eq!(fits.at(first).fit_id(), None);
    assert_eq!(fits.at(first + Duration::seconds(1)).fit_id(), Some(a_id.as_str()));
    assert_eq!(fits.at(second).fit_id(), Some(a_id.as_str()));
    assert_eq!(fits.at(second + Duration::seconds(1)).fit_id(), Some(b_id.as_str()));
    assert_eq!(fits.at(second + Duration::days(30)).fit_id(), Some(b_id.as_str()));
}

#[test]
fn a_walked_fitted_heuristic_answers_exactly_as_its_dated_registry() {
    let input = review_input();
    let history = StageSamples::default();
    let early = tagged(fit_as_of(), "early");
    // A file cut off after the estimate is never used for it.
    let late = tagged(input.as_of + Duration::hours(1), "late");
    let fits = DatedFits::new(vec![early.clone(), late]);
    let direct = Registry::with_fit(Some(Arc::new(early)));
    for id in [LAND_TWIN_OTTER_B, LAND_BRISK_PETREL, LAND_V2] {
        let walked = fits.heuristic(id).unwrap().estimate(&input, &history);
        let expected = direct.get(id).unwrap().estimate(&input, &history);
        assert_eq!(bytes(&walked), bytes(&expected), "{id}");
    }
    let answered = fits
        .heuristic(LAND_TWIN_OTTER_B)
        .unwrap()
        .estimate(&input, &history);
    assert!(answered.result.is_some(), "the fit answers the PR stage");
    // Before every file: the refusal live serving would give.
    let mut before = input.clone();
    before.as_of = fit_as_of();
    let refused = fits
        .heuristic(LAND_TWIN_OTTER_B)
        .unwrap()
        .estimate(&before, &history);
    assert_eq!(refused.no_estimate_reason, Some(NoEstimateReason::NoModel));
}

#[test]
fn load_dir_reads_every_fit_file_and_a_later_name_wins_a_tied_cutoff() {
    let dir = tempfile::tempdir().unwrap();
    let first = fit_as_of();
    let second = first + Duration::days(1);
    let tie_a = tagged(second, "tie-a");
    let tie_b = tagged(second, "tie-b");
    let winner = tie_b.id.clone();
    coeffs::write(&dir.path().join(coeffs::path_for(first)), &tagged(first, "x")).unwrap();
    coeffs::write(&dir.path().join("fit-2-a.json"), &tie_a).unwrap();
    coeffs::write(&dir.path().join("fit-2-b.json"), &tie_b).unwrap();
    // Neither a foreign schema nor a non-JSON file is a fit.
    std::fs::write(dir.path().join("other.json"), "{\"schema\":\"nope\"}").unwrap();
    std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
    let fits = DatedFits::load_dir(dir.path()).unwrap();
    assert_eq!(fits.cutoffs(), vec![first, second]);
    assert_eq!(fits.at(second + Duration::seconds(1)).fit_id(), Some(winner.as_str()));
    assert!(DatedFits::load_dir(&dir.path().join("missing")).is_err());
}
