//! Tests for the post-wait re-validation read.

use super::*;

const PRE: &str = "490b79f1d";

fn labels(raw: &str) -> String {
    match decide(raw, PRE) {
        Verdict::Labels(l) => l,
        other => panic!("expected Labels for {raw:?}, got {other:?}"),
    }
}

fn is_unreadable(raw: &str, pre: &str) -> bool {
    matches!(decide(raw, pre), Verdict::Unreadable(_))
}

#[test]
fn unchanged_head_hands_back_the_label_set_in_order() {
    let raw = r#"{"head":{"sha":"490b79f1d"},"merged":false,
        "labels":[{"name":"loom:pr"},{"name":"loom:changes-requested"}]}"#;
    assert_eq!(labels(raw), "loom:pr\nloom:changes-requested");
}

#[test]
fn merged_wins_over_everything_else_in_the_payload() {
    // The retired code returned before it ever looked at head or labels.
    for raw in [
        r#"{"merged":true}"#,
        r#"{"merged":true,"head":{"sha":"ab58dd87d"},"labels":"garbage"}"#,
        r#"{"merged":true,"head":"not an object"}"#,
    ] {
        assert_eq!(decide(raw, PRE), Verdict::Merged, "{raw}");
    }
}

#[test]
fn a_moved_head_wins_over_stale_labels() {
    // A rebase moves the head AND revokes loom:pr; the head is the exit-3
    // re-queue, never the exit-1 approval refusal.
    let raw = r#"{"head":{"sha":"ab58dd87d"},"merged":false,"labels":[{"name":"loom:review-requested"}]}"#;
    assert_eq!(decide(raw, PRE), Verdict::HeadMoved("ab58dd87d".into()));
    // …and even when the label set is unreadable, since it is never reached.
    let raw = r#"{"head":{"sha":"ab58dd87d"},"labels":"x"}"#;
    assert_eq!(decide(raw, PRE), Verdict::HeadMoved("ab58dd87d".into()));
}

#[test]
fn an_empty_precondition_skips_the_head_comparison() {
    let raw = r#"{"head":{"sha":"ab58dd87d"},"labels":[{"name":"loom:pr"}]}"#;
    assert_eq!(decide(raw, ""), Verdict::Labels("loom:pr".into()));
}

#[test]
fn sha_comparison_is_exact_not_case_folded_or_prefix() {
    let raw = r#"{"head":{"sha":"490B79F1D"},"labels":[]}"#;
    assert_eq!(decide(raw, PRE), Verdict::HeadMoved("490B79F1D".into()));
    let raw = r#"{"head":{"sha":"490b79f1d0"},"labels":[]}"#;
    assert_eq!(decide(raw, PRE), Verdict::HeadMoved("490b79f1d0".into()));
}

#[test]
fn every_missing_head_shape_is_unreadable_not_a_label_verdict() {
    // #8896: the re-read never happened, so the label set is unknown.
    for raw in [
        "{}",
        r#"{"head":null,"labels":[{"name":"loom:pr"}]}"#,
        r#"{"head":{},"labels":[]}"#,
        r#"{"head":{"sha":null},"labels":[]}"#,
        r#"{"head":{"sha":""},"labels":[]}"#,
    ] {
        assert!(is_unreadable(raw, PRE), "{raw}");
        assert!(is_unreadable(raw, ""), "{raw} (no precondition)");
    }
}

#[test]
fn payloads_jq_could_not_parse_are_unreadable() {
    // Each of these killed the retired function under set -e with jq's exit 5.
    for raw in [
        "",
        "garbage",
        "<html><body>502 Bad Gateway</body></html>",
        r#"{"head":{"sha":"490b79f1d"},"labels":["#,
        r#"{"head":"490b79f1d","labels":[]}"#,
        r#"[{"head":{"sha":"490b79f1d"}}]"#,
        "null",
    ] {
        assert!(is_unreadable(raw, PRE), "{raw:?}");
    }
}

#[test]
fn a_forge_error_body_followed_by_the_fallback_object_is_unreadable() {
    // `forge_get_pr_nocache … || echo '{}'` concatenates when gh prints an
    // error body and fails: two documents, neither of them a PR.
    let raw = "{\"message\":\"Server Error\"}\n{}";
    assert!(is_unreadable(raw, PRE));
    // Even when the first document LOOKS like a PR, a second one means the
    // read failed after printing — not a payload to trust.
    let raw = "{\"head\":{\"sha\":\"490b79f1d\"},\"labels\":[{\"name\":\"loom:pr\"}]}\n{}";
    assert!(is_unreadable(raw, PRE));
}

#[test]
fn a_partly_readable_label_set_is_unreadable_not_truncated() {
    // The retired `|| true` kept "loom:pr" and dropped the error, so the
    // guards ran against a label set with loom:changes-requested missing.
    let raw = r#"{"head":{"sha":"490b79f1d"},"labels":[{"name":"loom:pr"},"x",{"name":"loom:changes-requested"}]}"#;
    assert!(is_unreadable(raw, PRE));
    let raw = r#"{"head":{"sha":"490b79f1d"},"labels":[{"name":"loom:pr"},{"name":7}]}"#;
    assert!(is_unreadable(raw, PRE));
}

#[test]
fn a_missing_labels_key_is_unknown_but_null_is_empty() {
    // Absent: the retired code read "no labels", which --allow-unapproved
    // would have overridden with an audit comment instead of refusing.
    assert!(is_unreadable(r#"{"head":{"sha":"490b79f1d"}}"#, PRE));
    assert!(is_unreadable(r#"{"head":{"sha":"490b79f1d"},"labels":"loom:pr"}"#, PRE));
    // null is how a Go server (Gitea) renders an empty slice.
    assert_eq!(labels(r#"{"head":{"sha":"490b79f1d"},"labels":null}"#), "");
    assert_eq!(labels(r#"{"head":{"sha":"490b79f1d"},"labels":[]}"#), "");
}

#[test]
fn label_rendering_matches_jq_r_after_command_substitution() {
    // A name-less label is skipped (`.name // empty`).
    assert_eq!(
        labels(
            r#"{"head":{"sha":"490b79f1d"},"labels":[{"name":null},{"color":"f00"},{"name":"loom:pr"}]}"#
        ),
        "loom:pr"
    );
    // An empty-string name prints an empty line: kept in the middle…
    assert_eq!(
        labels(r#"{"head":{"sha":"490b79f1d"},"labels":[{"name":"a"},{"name":""},{"name":"b"}]}"#),
        "a\n\nb"
    );
    // …stripped at the end, with every other trailing newline.
    assert_eq!(
        labels(
            r#"{"head":{"sha":"490b79f1d"},"labels":[{"name":"a"},{"name":""},{"name":"b\n"}]}"#
        ),
        "a\n\nb"
    );
    // Duplicates and order are preserved — jq did not sort or unique here.
    assert_eq!(
        labels(r#"{"head":{"sha":"490b79f1d"},"labels":[{"name":"z"},{"name":"a"},{"name":"z"}]}"#),
        "z\na\nz"
    );
}

#[test]
fn merged_must_be_a_boolean_or_null() {
    assert!(is_unreadable(
        r#"{"merged":"true","head":{"sha":"490b79f1d"},"labels":[]}"#,
        PRE
    ));
    assert!(is_unreadable(r#"{"merged":1,"head":{"sha":"490b79f1d"},"labels":[]}"#, PRE));
    assert_eq!(labels(r#"{"merged":null,"head":{"sha":"490b79f1d"},"labels":[]}"#), "");
}

#[test]
fn a_non_hex_sha_is_unreadable_rather_than_compared() {
    for sha in [
        r#""490b79f1d\n""#,
        r#""490b 79f1d""#,
        "123",
        r#""HEAD""#,
        "true",
    ] {
        let raw = format!(r#"{{"head":{{"sha":{sha}}},"labels":[]}}"#);
        assert!(is_unreadable(&raw, PRE), "{raw}");
    }
}

#[test]
fn the_refusal_text_is_the_retired_one() {
    assert_eq!(
        unreadable_message("8220"),
        "Merge blocked: could not re-read PR #8220 after --auto's settle-wait — the uncached \
         re-read returned no usable payload (no head SHA), so neither the head nor the label set \
         could be re-validated against current state. This is a forge read failure, NOT a missing \
         `loom:pr` label: refusing to merge rather than treating an unreadable response as a \
         verdict. Re-run once the forge API is healthy."
    );
}
