//! Tests for blocker fingerprints (epic #7810, PR 3).
//!
//! These matter more than most: fingerprints are written into live issue
//! comments, so a divergence does not fail loudly — it makes Champion re-fight
//! decisions it already made.

use super::*;

#[test]
fn a_fingerprint_is_sixteen_hex_characters() {
    let f = fingerprint("o/r#3");
    assert_eq!(f.len(), 16, "got {f:?}");
    assert!(f.chars().all(|c| c.is_ascii_hexdigit()), "got {f:?}");
}

#[test]
fn order_does_not_matter() {
    // The whole point: the same SET of blockers is the same decision.
    assert_eq!(fingerprint("o/r#3 o/r#9"), fingerprint("o/r#9 o/r#3"));
}

#[test]
fn duplicates_do_not_matter() {
    assert_eq!(fingerprint("o/r#3 o/r#3 o/r#9"), fingerprint("o/r#3 o/r#9"));
}

#[test]
fn any_whitespace_run_separates_nodes() {
    // The shell's unquoted `$nodes` splits on IFS, so newlines and runs of
    // spaces behave identically to single spaces.
    let want = fingerprint("o/r#3 o/r#9");
    assert_eq!(fingerprint("o/r#3\no/r#9"), want);
    assert_eq!(fingerprint("  o/r#3   o/r#9  "), want);
    assert_eq!(fingerprint("o/r#3\t o/r#9\n"), want);
}

#[test]
fn different_sets_differ() {
    assert_ne!(fingerprint("o/r#3"), fingerprint("o/r#4"));
    assert_ne!(fingerprint("o/r#3"), fingerprint("o/r#3 o/r#4"));
}

#[test]
fn an_empty_set_is_stable() {
    // Hash of the empty string; the shell reaches the same place via an empty
    // `nodes` and the trailing-space strip.
    assert_eq!(fingerprint(""), fingerprint("   "));
    assert_eq!(fingerprint("").len(), 16);
}

#[test]
fn a_fact_fingerprint_is_prefixed_and_keyed_on_both_inputs() {
    let f = fact_fingerprint("escalation text", "abc1234");
    assert!(f.starts_with("fact-"), "got {f:?}");
    assert_eq!(f.len(), "fact-".len() + 16);

    assert_ne!(
        fact_fingerprint("escalation text", "abc1234"),
        fact_fingerprint("escalation text", "def5678"),
        "a different resolving commit must produce a different identifier"
    );
    assert_ne!(
        fact_fingerprint("a", "abc1234"),
        fact_fingerprint("b", "abc1234"),
        "different escalation text must produce a different identifier"
    );
}

// ---------------------------------------------------------------------------
// The differential tests that used to live here (epic #7810, PR 3)
// ---------------------------------------------------------------------------
//
// This port was not translated on trust. Each function above landed in #7943
// beside a DIFFERENTIAL test that ran it and the shell original over the same
// fixture corpus and asserted they agreed, character for character, with an
// anti-vacuity guard so a shell that silently produced nothing could not pass.
//
// Those tests are removed here with the shell they compared against: a
// comparison needs both sides, and keeping a copy of the retired
// implementation purely to compare with would be keeping the thing this epic
// retires. The evidence is the merged CI run on #7943, not a fixture that
// pins a deleted file forever.
//
// What still runs both ways is the black-box suite
// `defaults/scripts/tests/test-classify-dependency-block.sh`, whose assertions
// were written against the shell and now drive this implementation unchanged
// through the same CLI.
