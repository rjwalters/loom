//! Tests for the CLI boundary (epic #7810, PR 4).
//!
//! Thin by design: argv, stdout and exit codes are proven end to end by
//! `defaults/scripts/tests/test-dep-recheck-fingerprint.sh`, whose assertions
//! were written against the shell. What is here is the one property that suite
//! only exercises for a handful of fixtures — that what a caller `eval`s gets
//! the value back.
//!
//! These assert the **round trip through real bash**, not byte-equality with
//! bash's own `%q` spelling. `%q` picks backslash escaping where this picks
//! quotes; both parse to the same value, and the value is what the contract is
//! about. Pinning the spelling would fail on a cosmetic difference and pass on
//! a value that silently changed.

use super::*;

/// `eval` the assignment the way a real consumer does, and return what the
/// variable holds.
///
/// The assignment is PIPED IN and evaluated as `eval "$(cat)"` — byte for byte
/// what `test-dep-recheck-fingerprint.sh` and `curator.md` do. Embedding it in
/// a double-quoted `eval "REFS=..."` argument instead would expand `$(...)` and
/// `$VAR` while building that argument, before the quoting is ever consulted,
/// and the test would report an injection this code never had.
fn eval_roundtrip(value: &str) -> String {
    use std::io::Write as _;
    let assignment = format!("REFS={}\n", shell_quote(value));
    let mut child = std::process::Command::new("bash")
        .arg("-euc")
        .arg(r#"eval "$(cat)"; printf '%s' "$REFS""#)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("bash");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(assignment.as_bytes())
        .expect("write");
    let out = child.wait_with_output().expect("bash");
    assert!(
        out.status.success(),
        "eval failed for {value:?}: quoted as {} — stderr: {}",
        shell_quote(value),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn every_shape_a_refs_list_can_take_survives_eval() {
    for value in [
        "",                   // no references — the common case
        "7",                  // one
        "7 9 10",             // several, space-joined (extract-refs)
        "3:OPEN\n9:CLOSED",   // multi-line (operator-premise)
        "42:OPEN\n43:CLOSED", // the suite's own T29 fixture
    ] {
        assert_eq!(eval_roundtrip(value), value, "{value:?}");
    }
}

#[test]
fn an_empty_value_quotes_to_two_quotes_rather_than_nothing() {
    // `REFS=` parses, but emitting nothing at all is a shape a caller can
    // misread; bash's own %q emits '' here too.
    assert_eq!(shell_quote(""), "''");
    assert_eq!(eval_roundtrip(""), "");
}

#[test]
fn a_value_that_tries_to_close_its_own_quoting_cannot() {
    // The injection shape. These can only arrive through a forge `state` string
    // today, but the quoting is what stands between forge text and a caller's
    // shell, so it is tested on values that would exploit a naive one.
    for value in [
        "it's",
        "'; echo pwned; '",
        "$(echo pwned)",
        "`echo pwned`",
        "a\\b",
        "a\\b\nc",
        "x\"y",
        "$HOME",
    ] {
        assert_eq!(eval_roundtrip(value), value, "{value:?}");
    }
}

#[test]
fn a_safe_word_is_left_bare() {
    // Not required for correctness, but it is what the existing fixtures see,
    // and gratuitously quoting every value would churn the suite's expectations.
    assert_eq!(shell_quote("7"), "7");
}

// ---------------------------------------------------------------------------
// `--refs` token parsing (#8011)
// ---------------------------------------------------------------------------

#[test]
fn every_refs_token_must_parse_as_a_number() {
    // The shell original iterated `for ref in $REFS_ARG` and `_die`d (exit 1)
    // the moment a token failed both `gh issue view`/`gh pr view` — which a
    // non-numeric token always would. Silently dropping it instead (as a bare
    // `.filter_map(|t| t.parse().ok())` does) computes a fingerprint over
    // fewer references than the caller asked for: a confident wrong answer.
    assert!(super::parse_refs_arg("abc").is_err());
    assert!(super::parse_refs_arg("123 abc").is_err());
    assert!(super::parse_refs_arg("abc 123").is_err());
}

#[test]
fn a_refs_list_of_valid_numbers_parses_in_order() {
    assert_eq!(super::parse_refs_arg("123 456").unwrap(), vec![123, 456]);
    assert_eq!(super::parse_refs_arg("").unwrap(), Vec::<i64>::new());
}

#[test]
fn a_backslash_in_a_multi_line_value_is_escaped_before_the_newline_is() {
    // Order matters inside the `$'...'` form: escaping the newline first and
    // the backslash second would double-escape the inserted \n and yield a
    // literal backslash-n. The round trip above is what catches it; this pins
    // the reason.
    assert_eq!(shell_quote("a\\b\nc"), "$'a\\\\b\\nc'");
}
