//! Tests for the release-build check (#10718). No network: the tag lookup is
//! a closure.

use std::cell::Cell;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};

use super::*;

const COMMIT: &str = "1111111111111111111111111111111111111111";
const OTHER: &str = "2222222222222222222222222222222222222222";
const INTERVAL: Duration = Duration::from_secs(60);

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
}

#[test]
fn the_stamp_must_name_this_version_a_known_commit_and_a_clean_tree() {
    assert_eq!(stamp_check("v0.19.925", "0.19.925", COMMIT, "clean"), Ok("v0.19.925".into()));
    // No stamp: every build the release workflow did not make.
    assert!(stamp_check("", "0.19.925", COMMIT, "clean")
        .unwrap_err()
        .contains("not built by the release workflow"));
    // A stamp for another version, a missing commit, a modified tree.
    assert!(stamp_check("v0.19.924", "0.19.925", COMMIT, "clean").is_err());
    assert!(stamp_check("0.19.925", "0.19.925", COMMIT, "clean").is_err());
    assert!(stamp_check("v0.19.925", "0.19.925", "unknown", "clean").is_err());
    assert!(stamp_check("v0.19.925", "0.19.925", COMMIT, "dirty").is_err());
    assert!(stamp_check("v0.19.925", "0.19.925", COMMIT, "unknown").is_err());
}

#[test]
fn an_unstamped_build_is_never_a_release_and_never_asks_the_forge() {
    let mut checker = Checker::new("", "0.19.925", COMMIT, "clean");
    let asked = Cell::new(0);
    let api = |_: &str, _: &str| -> Result<String, String> {
        asked.set(asked.get() + 1);
        Ok(COMMIT.to_string())
    };
    let found = checker.ensure(t0(), INTERVAL, &api);
    assert!(matches!(found, Provenance::NotStamped(_)), "{found:?}");
    assert!(found.refuted());
    assert_eq!(asked.get(), 0);
}

#[test]
fn a_stamped_build_is_verified_once_the_tag_names_its_commit() {
    let mut checker = Checker::new("v0.19.925", "0.19.925", COMMIT, "clean");
    // Before any lookup it is unverified, not refuted.
    let before = checker.current();
    assert!(matches!(before, Provenance::Unverified { .. }) && !before.refuted());
    let asked = Cell::new(0);
    let api = |repo: &str, tag: &str| -> Result<String, String> {
        asked.set(asked.get() + 1);
        assert_eq!((repo, tag), ("rjwalters/loom", "v0.19.925"));
        Ok(COMMIT.to_string())
    };
    assert_eq!(checker.ensure(t0(), INTERVAL, &api), Provenance::Verified);
    // Cached: no second lookup.
    assert_eq!(checker.ensure(t0(), INTERVAL, &api), Provenance::Verified);
    assert_eq!(asked.get(), 1);
}

#[test]
fn a_stamp_set_by_hand_on_another_commit_is_refuted_for_good() {
    // The stamp is only an environment variable: a feature-branch build that
    // sets it still fails, because the tag names the released commit.
    let mut checker = Checker::new("v0.19.925", "0.19.925", OTHER, "clean");
    let asked = Cell::new(0);
    let api = |_: &str, _: &str| -> Result<String, String> {
        asked.set(asked.get() + 1);
        Ok(COMMIT.to_string())
    };
    let found = checker.ensure(t0(), INTERVAL, &api);
    assert_eq!(
        found,
        Provenance::Mismatch {
            tag_commit: COMMIT.to_string()
        }
    );
    assert!(found.refuted());
    checker.ensure(t0() + ChronoDuration::hours(5), INTERVAL, &api);
    assert_eq!(asked.get(), 1, "a mismatch is final");
}

#[test]
fn a_failed_lookup_is_unverified_and_retried_with_backoff() {
    let mut checker = Checker::new("v0.19.925", "0.19.925", COMMIT, "clean");
    let asked = Cell::new(0);
    let up = Cell::new(false);
    let api = |_: &str, _: &str| -> Result<String, String> {
        asked.set(asked.get() + 1);
        if up.get() {
            Ok(COMMIT.to_string())
        } else {
            Err("`gh api` exited 1".to_string())
        }
    };
    let at = |secs: i64| t0() + ChronoDuration::seconds(secs);
    // First failure: retry one interval later.
    let found = checker.ensure(at(0), INTERVAL, &api);
    let Provenance::Unverified { why, retry_at } = &found else {
        panic!("{found:?}");
    };
    assert!(why.contains("gh api"), "{why}");
    assert_eq!(*retry_at, Some(at(60)));
    assert!(!found.refuted());
    // Not due yet: no lookup.
    checker.ensure(at(59), INTERVAL, &api);
    assert_eq!(asked.get(), 1);
    // Second failure doubles the wait, the third doubles it again.
    checker.ensure(at(60), INTERVAL, &api);
    checker.ensure(at(179), INTERVAL, &api);
    assert_eq!(asked.get(), 2);
    let third = checker.ensure(at(180), INTERVAL, &api);
    assert_eq!(asked.get(), 3);
    assert!(matches!(third, Provenance::Unverified { retry_at, .. } if retry_at == Some(at(420))));
    // The wait is capped: walk the backoff well past the cap, then measure
    // one more step.
    let due = |checker: &Checker| match checker.current() {
        Provenance::Unverified {
            retry_at: Some(at), ..
        } => at,
        other => panic!("still unverified, with a retry time: {other:?}"),
    };
    for _ in 0..12 {
        let at = due(&checker);
        checker.ensure(at, INTERVAL, &api);
    }
    let last = due(&checker);
    let before = asked.get();
    checker.ensure(last, INTERVAL, &api);
    assert_eq!(asked.get(), before + 1);
    let retry_at = due(&checker);
    assert_eq!(retry_at - last, ChronoDuration::from_std(RETRY_CAP).unwrap());
    // The forge comes back: verified on the next due lookup.
    up.set(true);
    assert_eq!(checker.ensure(retry_at, INTERVAL, &api), Provenance::Verified);
}

/// The build under test is a CI or developer build of a branch: it carries no
/// release stamp, so it is not a release build and its payload is never
/// pushed. Nothing in the test suite can verify it either, because only
/// [`ensure`] asks the forge and no test calls it on the process-wide checker.
#[test]
fn this_test_build_is_not_a_release_build() {
    assert!(!is_verified());
    if built_release_tag().is_empty() {
        assert!(matches!(current(), Provenance::NotStamped(_)), "{:?}", current());
    }
    let stamp = crate::init::payload::Stamp::this_binary().unwrap();
    assert!(!stamp.release_build);
}
