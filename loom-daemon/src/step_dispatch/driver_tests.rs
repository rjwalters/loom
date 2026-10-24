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
    assert_eq!(shape_pool(&mut fresh_pool, &DrawClaims::default(), fresh()), 0);
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
        assert_eq!(shape_pool(&mut w, &DrawClaims::default(), listing), 1, "{listing:?}");
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
    let _ = shape_pool(&mut w, &DrawClaims::default(), fresh());
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

/// #12025: the step path's listing failures reach `loom.dispatch.error_kind`.
/// A failed item listing is a listing failure of its kind; a last-good item
/// listing is an incomplete one; held Builder steps are one `forge_listing`
/// error per repo; a fresh, whole tick records nothing.
#[test]
fn listing_failures_and_held_builders_name_their_error_kind() {
    let mut report = TickReport::default();
    note_listing(&mut report, 0, None, Some(fresh()), 0);
    assert_eq!((report.errors, report.error_kind()), (0, None));
    assert!(report.listing_not_whole().is_empty());

    let down = anyhow::anyhow!("gh: HTTP 502 Bad Gateway");
    note_listing(&mut report, 0, Some(&down), None, 0);
    let stale = ListingState {
        stale_listing: true,
        ..fresh()
    };
    note_listing(&mut report, 1, None, Some(stale), 1);
    let blind = ListingState {
        pulls_complete: false,
        ..fresh()
    };
    note_listing(&mut report, 2, None, Some(blind), 2);
    assert_eq!(report.errors, 3, "one per failed listing, one per repo holding Builders");
    assert_eq!(report.error_kind(), Some("forge_listing"));
    assert_eq!(report.listing_failed, vec![0]);
    assert_eq!(report.listing_incomplete, vec![1]);

    let mut limited = TickReport::default();
    let wall = anyhow::anyhow!("gh: API rate limit exceeded (HTTP 403)");
    note_listing(&mut limited, 0, Some(&wall), None, 0);
    assert_eq!(limited.error_kind(), Some("rate_limited"));
}

/// A red repo held for its fixes keeps every step that adds no code to `main`
/// for a non-fix item, and drops Builder and `ChampionMerge` unless the item
/// is a red-main fix (#12046).
#[test]
fn a_fixes_only_listing_keeps_the_steps_that_add_no_code() {
    let held = ListingState {
        fixes_only: true,
        ..fresh()
    };
    let steps = |w: &crate::step_dispatch::item_draw::Weighing| -> Vec<Step> {
        w.pool.iter().map(|c| c.step).collect()
    };

    let mut open = workspace(0, "acme/one", &forge()).weighing;
    let before = steps(&open);
    for step in [
        Step::Builder,
        Step::ChampionMerge,
        Step::Judge,
        Step::Doctor,
    ] {
        assert!(before.contains(&step), "fixture lacks {step:?}");
    }
    let _ = shape_pool(&mut open, &DrawClaims::default(), fresh());
    assert_eq!(steps(&open), before, "fixes_only: false is unchanged");

    let mut w = workspace(0, "acme/one", &forge()).weighing;
    let _ = shape_pool(&mut w, &DrawClaims::default(), held);
    let kept = steps(&w);
    assert!(!kept.contains(&Step::Builder) && !kept.contains(&Step::ChampionMerge));
    let expected: Vec<Step> = before
        .iter()
        .copied()
        .filter(|s| !matches!(s, Step::Builder | Step::ChampionMerge))
        .collect();
    assert_eq!(kept, expected, "every other step stays");

    let mut fix = workspace(0, "acme/one", &forge()).weighing;
    for c in &mut fix.pool {
        c.item.lane = Lane::RedMain;
    }
    let _ = shape_pool(&mut fix, &DrawClaims::default(), held);
    assert_eq!(steps(&fix), before, "red-main fixes keep every step");
}
