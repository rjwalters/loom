//! Tests for version parsing and comparison (epic #7810, PR 5).

use super::*;

#[test]
fn a_release_tag_yields_its_version() {
    assert_eq!(extract_version("v0.19.24").as_deref(), Some("0.19.24"));
}

#[test]
fn a_version_banner_yields_its_version_and_commit() {
    let banner = "loom-daemon 0.19.24 (commit a1b2c3d, built 2026-09-16)";
    assert_eq!(extract_version(banner).as_deref(), Some("0.19.24"));
    assert_eq!(extract_commit(banner).as_deref(), Some("a1b2c3d"));
}

#[test]
fn only_the_first_match_counts() {
    assert_eq!(extract_version("0.19.24 supersedes 0.19.21").as_deref(), Some("0.19.24"));
}

#[test]
fn text_with_no_version_yields_none_rather_than_a_default() {
    // A fabricated 0.0.0 here would compare as older than everything and make
    // the daemon roll on a host whose binary simply did not answer.
    assert_eq!(extract_version("loom-daemon (unknown build)"), None);
    assert_eq!(extract_commit("loom-daemon 0.19.24"), None);
}

#[test]
fn ordering_is_component_wise_not_lexicographic() {
    use std::cmp::Ordering::*;
    assert_eq!(compare("0.19.9", "0.19.10"), Less, "9 < 10, not '9' > '1'");
    assert_eq!(compare("0.20.0", "0.9.99"), Greater);
    assert_eq!(compare("1.0.0", "0.99.99"), Greater);
}

#[test]
fn equal_versions_compare_equal() {
    assert_eq!(compare("0.19.24", "0.19.24"), std::cmp::Ordering::Equal);
}

#[test]
fn a_missing_component_defaults_to_zero() {
    use std::cmp::Ordering::*;
    assert_eq!(compare("0.19", "0.19.0"), Equal);
    assert_eq!(compare("0.19", "0.19.1"), Less);
}

#[test]
fn a_non_numeric_suffix_is_stripped_rather_than_failing() {
    use std::cmp::Ordering::*;
    assert_eq!(compare("0.19.24-dirty", "0.19.24"), Equal);
    assert_eq!(compare("v0.19.24", "0.19.24"), Equal, "a leading v parses as 0");
}

#[test]
fn an_unparseable_component_sorts_as_zero_not_as_newer() {
    // The failure direction that matters: garbage must never look like an
    // upgrade, or the daemon rolls onto a version it could not read.
    use std::cmp::Ordering::*;
    assert_eq!(compare("junk", "0.0.1"), Less);
    assert_eq!(compare("", "0.0.0"), Equal);
}

#[test]
fn a_fourth_component_is_ignored() {
    assert_eq!(compare("1.2.3.4", "1.2.3.9"), std::cmp::Ordering::Equal);
}
