//! #9247: each of the base-conflict flag's two writes re-mints and retries
//! once, on its own, when refused as not accessible by the integration.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::stale_token_retry::set_test_refresh;
use crate::stale_token_retry::tests::{calls, counting_refresh, refusing_gh, REFUSED};

const SHA: &str = "5555555555555555555555555555555555555555";

#[test]
fn both_flag_writes_land_each_on_its_own_retry() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = refusing_gh(dir.path(), false);
    let refreshed = counting_refresh(true);
    flag(&gh, dir.path(), 8909, SHA).unwrap();
    set_test_refresh(None);
    assert_eq!(calls(&log, "pr comment 8909"), 2, "the comment: refused, then landed");
    assert_eq!(calls(&log, "pr edit 8909"), 2, "the relabel: refused, then landed");
    assert_eq!(refreshed.get(), 2, "one refresh per write");
    let lines = std::fs::read_to_string(&log).unwrap();
    let last = lines.lines().last().unwrap();
    assert!(
        last.contains("--remove-label loom:review-requested")
            && last.contains("--add-label loom:changes-requested")
            && last.contains("--add-label loom:merge-conflict"),
        "{last}"
    );
}

/// A comment refused twice aborts the flag before any label is touched, with
/// the first refusal as its error.
#[test]
fn a_comment_refused_twice_aborts_before_the_relabel() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = refusing_gh(dir.path(), true);
    let refreshed = counting_refresh(true);
    let err = flag(&gh, dir.path(), 8909, SHA).unwrap_err();
    set_test_refresh(None);
    assert!(err.to_string().contains(REFUSED), "{err}");
    assert_eq!((calls(&log, "pr comment 8909"), calls(&log, "pr edit 8909")), (2, 0));
    assert_eq!(refreshed.get(), 1);
}

/// Scope: `clear` is not one of the three sites and keeps its single attempt.
#[test]
fn the_clear_path_is_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = refusing_gh(dir.path(), false);
    let refreshed = counting_refresh(true);
    assert!(clear(&gh, dir.path(), 8909).is_err());
    set_test_refresh(None);
    assert_eq!((calls(&log, "pr edit 8909"), refreshed.get()), (1, 0));
}
