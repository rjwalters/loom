//! #9247: the verdict anchor comment re-mints and retries once when it is
//! refused as not accessible by the integration.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::stale_token_retry::set_test_refresh;
use crate::stale_token_retry::tests::{calls, counting_refresh, refusing_gh, REFUSED};

const SHA: &str = "4444444444444444444444444444444444444444";

fn unmarked() -> VerdictPr {
    VerdictPr {
        number: 300,
        kind: VerdictKind::ChangesRequested,
        head_sha: Some(SHA.to_string()),
        marker_sha: None,
        marker_scan_ok: true,
        on_hold: false,
        invalidation_recorded: false,
    }
}

#[test]
fn a_refused_anchor_comment_lands_on_the_one_retry() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = refusing_gh(dir.path(), false);
    let refreshed = counting_refresh(true);
    anchor_verdict(&gh, dir.path(), &unmarked(), SHA).unwrap();
    set_test_refresh(None);
    assert_eq!(calls(&log, "pr comment 300"), 2, "refused once, then landed");
    assert_eq!(refreshed.get(), 1);
}

#[test]
fn an_anchor_refused_twice_reports_the_first_refusal_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = refusing_gh(dir.path(), true);
    let refreshed = counting_refresh(true);
    let err = anchor_verdict(&gh, dir.path(), &unmarked(), SHA).unwrap_err();
    set_test_refresh(None);
    assert!(err.to_string().contains(REFUSED), "{err}");
    assert_eq!((calls(&log, "pr comment 300"), refreshed.get()), (2, 1));
}

#[test]
fn an_anchor_is_not_rerun_when_no_fresh_token_could_be_had() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = refusing_gh(dir.path(), false);
    let refreshed = counting_refresh(false);
    assert!(anchor_verdict(&gh, dir.path(), &unmarked(), SHA).is_err());
    set_test_refresh(None);
    assert_eq!((calls(&log, "pr comment 300"), refreshed.get()), (1, 1));
}
