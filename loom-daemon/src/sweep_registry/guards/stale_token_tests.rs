//! #9247: the dispatch claim flip re-mints and retries once when it is
//! refused as not accessible by the integration.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::stale_token_retry::set_test_refresh;
use crate::stale_token_retry::tests::{calls, counting_refresh, refusing_gh, REFUSED};

const FLIP: &str = "issue edit 9247 --remove-label loom:issue --add-label loom:building";

fn registry(ws: &Path, gh: PathBuf) -> SweepRegistry {
    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.gh_bin = Some(gh);
    config.skip_label_flip = false;
    SweepRegistry::new(config)
}

#[test]
fn a_refused_claim_flip_lands_on_the_one_retry() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = refusing_gh(dir.path(), false);
    let refreshed = counting_refresh(true);
    registry(dir.path(), gh)
        .flip_label_to_building(9247)
        .unwrap();
    set_test_refresh(None);
    assert_eq!(calls(&log, FLIP), 2, "refused once, then landed");
    assert_eq!(refreshed.get(), 1);
}

#[test]
fn a_flip_refused_twice_reports_the_first_refusal_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = refusing_gh(dir.path(), true);
    let refreshed = counting_refresh(true);
    let err = registry(dir.path(), gh)
        .flip_label_to_building(9247)
        .unwrap_err();
    set_test_refresh(None);
    assert!(err.to_string().contains(REFUSED), "{err}");
    assert_eq!((calls(&log, FLIP), refreshed.get()), (2, 1));
}

#[test]
fn a_flip_is_not_rerun_when_no_fresh_token_could_be_had() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = refusing_gh(dir.path(), false);
    let refreshed = counting_refresh(false);
    assert!(registry(dir.path(), gh)
        .flip_label_to_building(9247)
        .is_err());
    set_test_refresh(None);
    assert_eq!((calls(&log, FLIP), refreshed.get()), (1, 1));
}
