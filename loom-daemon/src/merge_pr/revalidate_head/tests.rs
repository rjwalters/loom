//! Unit tests for the post-wait re-read decision. The differential
//! (`tests/merge_pr_revalidate_head_differential.rs`) compares against the
//! frozen retired shell; these name the individual properties.

use super::*;

const PRE: &str = "490b79f1d";

#[test]
fn a_concurrently_merged_pr_short_circuits() {
    let p = r#"{"merged":true,"head":{"sha":"ab58dd87d"},"labels":[]}"#;
    assert_eq!(revalidate(p, PRE), Revalidation::Merged);
}

#[test]
fn an_unreadable_payload_is_no_head_never_moved() {
    for p in [
        "{}",
        "",
        "not json",
        r#"{"head":{"sha":null}}"#,
        r#"{"head":{"sha":""}}"#,
    ] {
        assert_eq!(revalidate(p, PRE), Revalidation::NoHead, "{p:?}");
    }
}

#[test]
fn a_moved_head_reports_the_fresh_sha() {
    let p = r#"{"merged":false,"head":{"sha":"ab58dd87d"},"labels":[{"name":"loom:pr"}]}"#;
    assert_eq!(
        revalidate(p, PRE),
        Revalidation::Moved {
            fresh_sha: "ab58dd87d".into()
        }
    );
}

#[test]
fn a_moved_head_wins_over_stale_labels() {
    let p = r#"{"head":{"sha":"ab58dd87d"},"labels":[{"name":"loom:review-requested"}]}"#;
    assert!(matches!(revalidate(p, PRE), Revalidation::Moved { .. }));
}

#[test]
fn no_precondition_never_reads_as_moved() {
    let p = r#"{"head":{"sha":"ab58dd87d"},"labels":[{"name":"loom:pr"}]}"#;
    assert_eq!(
        revalidate(p, ""),
        Revalidation::Clear {
            labels: vec!["loom:pr".into()]
        }
    );
}

#[test]
fn an_unchanged_head_returns_the_current_labels() {
    let p = r#"{"merged":false,"head":{"sha":"490b79f1d"},"labels":[{"name":"loom:pr"},{"name":"x"},{"name":null},{}]}"#;
    assert_eq!(
        revalidate(p, PRE),
        Revalidation::Clear {
            labels: vec!["loom:pr".into(), "x".into()]
        }
    );
}
