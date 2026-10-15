//! The driver's per-workspace shaping, through the same functions its tick
//! loop calls.

use super::*;
use crate::step_dispatch::item_draw::Exclusion;
use crate::step_dispatch::tick::tests::{forge, workspace};
use crate::sweep_registry::step_spawn::draw_claims::DrawClaims;
use crate::work_finder::step_item::{PassReason, Step};

fn fresh() -> ListingState {
    ListingState {
        pulls_complete: true,
        stale_listing: false,
        fixes_only: false,
    }
}

fn builders(w: &crate::step_dispatch::item_draw::Weighing) -> usize {
    w.pool.iter().filter(|c| c.step == Step::Builder).count()
}

/// A Builder claim is a blind label flip: a last-good listing, or a failed
/// open-PR listing, draws no Builder step; a fresh listing does (#11909).
#[test]
fn a_stale_or_pr_blind_listing_draws_no_builder_step() {
    let mut fresh_pool = workspace(0, "acme/one", &forge()).weighing;
    shape_pool(&mut fresh_pool, &DrawClaims::default(), fresh());
    assert_eq!(builders(&fresh_pool), 1, "#3 is a Builder step");

    for listing in [
        ListingState {
            stale_listing: true,
            ..fresh()
        },
        ListingState {
            pulls_complete: false,
            ..fresh()
        },
    ] {
        let mut w = workspace(0, "acme/one", &forge()).weighing;
        let others = w.pool.len() - builders(&w);
        shape_pool(&mut w, &DrawClaims::default(), listing);
        assert_eq!(builders(&w), 0, "{listing:?}");
        assert_eq!(w.pool.len(), others, "only the Builder step leaves: {listing:?}");
        assert_eq!(w.excluded[&Exclusion::Pass(PassReason::SharedPr)], 1);
    }
}

/// The actionable gauge is the shaped pool's size for a listed repo, and no
/// point at all for one the tick did not list (absent, never a 0).
#[test]
fn an_unlisted_repo_has_no_actionable_point() {
    let mut w = workspace(0, "acme/one", &forge()).weighing;
    shape_pool(&mut w, &DrawClaims::default(), fresh());
    let point = actionable_point("acme/one", Some(&w)).unwrap();
    assert_eq!(point.name, MetricName::StepActionable);
    let json = serde_json::to_value(&point).unwrap().to_string();
    assert!(json.contains("acme/one"), "{json}");
    assert!(actionable_point("acme/held", None).is_none());
    // A listed repo with nothing drawable is a measured 0, not absent.
    let mut empty = w.clone();
    empty.pool.clear();
    assert!(actionable_point("acme/one", Some(&empty)).is_some());
}
