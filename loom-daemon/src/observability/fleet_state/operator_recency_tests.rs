//! Operator-action recency and hold-release burst markers (#11956).

use chrono::{Duration, TimeZone, Utc};

use super::super::{FleetPrCensus, FleetStage, FleetStateRow, RepoView};
use super::{Track, BURST_MIN_RELEASES, BURST_WINDOW_MINUTES};
use crate::telemetry::kinds::fleet_state::FleetHoldChurn;

fn t0() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

/// A repo with `held` merge_hold rows and `open` rows in merge_wait.
fn repo(held: &[u32], open: &[u32], census: bool) -> RepoView {
    let mut repo = RepoView::default();
    if census {
        repo.census = Some(FleetPrCensus::default());
    }
    for n in held {
        repo.rows
            .insert(*n, FleetStateRow::new(*n, FleetStage::MergeHold, t0()));
    }
    for n in open {
        repo.rows
            .insert(*n, FleetStateRow::new(*n, FleetStage::MergeWait, t0()));
    }
    repo
}

#[test]
fn first_pass_has_no_markers() {
    let mut track = Track::default();
    track.observe(&repo(&[1, 2], &[], true), t0());
    assert_eq!(track.markers(t0()), None);
    // No churn is observable until a second pass diffs against the baseline.
    assert_eq!(track.churn(), None);
}

#[test]
fn a_release_after_a_hold_sets_recency_without_a_burst() {
    let mut track = Track::default();
    track.observe(&repo(&[1, 2], &[], true), t0());
    let later = t0() + Duration::minutes(5);
    // Issue 1 is still open but no longer held; issue 2 stays held.
    track.observe(&repo(&[2], &[1], true), later);
    let markers = track.markers(later + Duration::hours(2)).unwrap();
    assert_eq!(markers.last_hold_release_at, later);
    assert_eq!(markers.hold_release_burst, None);
    // The release is counted as an operator action, not a left-census event.
    assert_eq!(
        track.churn(),
        Some(FleetHoldChurn {
            releases: 1,
            left_census: 0,
        })
    );
}

#[test]
fn enough_releases_in_the_window_set_a_burst_and_it_expires() {
    let held: Vec<u32> = (1..=BURST_MIN_RELEASES).collect();
    let mut track = Track::default();
    track.observe(&repo(&held, &[], true), t0());
    let later = t0() + Duration::minutes(1);
    track.observe(&repo(&[], &held, true), later);
    let burst = track.markers(later).unwrap().hold_release_burst.unwrap();
    assert_eq!(burst.count_in_window, BURST_MIN_RELEASES);
    assert_eq!(burst.window_minutes, BURST_WINDOW_MINUTES);
    let after = later + Duration::minutes(i64::from(BURST_WINDOW_MINUTES) + 1);
    let markers = track.markers(after).unwrap();
    assert_eq!(markers.hold_release_burst, None, "aged out of the window");
    assert_eq!(markers.last_hold_release_at, later, "recency persists");
}

#[test]
fn one_fewer_than_the_threshold_is_no_burst() {
    let held: Vec<u32> = (1..BURST_MIN_RELEASES).collect();
    let mut track = Track::default();
    track.observe(&repo(&held, &[], true), t0());
    track.observe(&repo(&[], &held, true), t0() + Duration::minutes(1));
    let markers = track.markers(t0() + Duration::minutes(1)).unwrap();
    assert_eq!(markers.hold_release_burst, None);
}

#[test]
fn an_unread_census_is_unknown_not_zero_and_reseeds_the_baseline() {
    let mut track = Track::default();
    track.observe(&repo(&[1], &[], true), t0());
    // Census unread: the held set is unknown, so nothing is diffed.
    track.observe(&repo(&[], &[1], false), t0() + Duration::minutes(1));
    assert_eq!(track.markers(t0() + Duration::minutes(1)), None);
    // The next whole pass diffs against nothing, not against the stale set.
    track.observe(&repo(&[], &[1], true), t0() + Duration::minutes(2));
    assert_eq!(track.markers(t0() + Duration::minutes(2)), None);
}

#[test]
fn a_row_that_left_the_view_merged_or_closed_is_not_a_release() {
    let mut track = Track::default();
    track.observe(&repo(&[1], &[], true), t0());
    track.observe(&repo(&[], &[], true), t0() + Duration::minutes(1));
    assert_eq!(track.markers(t0() + Duration::minutes(1)), None);
    // It is still observable as a left-census event, so the "operator release
    // vs. merge/close" ambiguity is diagnosable even with no release (#12059).
    assert_eq!(
        track.churn(),
        Some(FleetHoldChurn {
            releases: 0,
            left_census: 1,
        })
    );
}

#[test]
fn releases_and_left_census_accumulate_independently() {
    let mut track = Track::default();
    track.observe(&repo(&[1, 2, 3], &[], true), t0());
    // Issue 1 released (still an open row), issues 2 and 3 left the census.
    track.observe(&repo(&[], &[1], true), t0() + Duration::minutes(1));
    assert_eq!(
        track.churn(),
        Some(FleetHoldChurn {
            releases: 1,
            left_census: 2,
        })
    );
    // A later release adds to the running release count, census count unchanged.
    track.observe(&repo(&[4], &[1], true), t0() + Duration::minutes(2));
    track.observe(&repo(&[], &[1, 4], true), t0() + Duration::minutes(3));
    assert_eq!(
        track.churn(),
        Some(FleetHoldChurn {
            releases: 2,
            left_census: 2,
        })
    );
}
