//! Tests for the typed command/forge boundary (epic #7810, PR 2).
//!
//! The point of this layer is that outcomes which the three retired runners
//! merged stay apart. So most of these assert a *distinction* — that two
//! situations produce different values — rather than that one situation
//! produces the right value.

use super::*;
use serde::Deserialize;
use std::path::PathBuf;

fn tmp() -> PathBuf {
    std::env::temp_dir()
}

const QUICK: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize, PartialEq)]
struct Pr {
    number: u64,
    state: String,
}

// ---------------------------------------------------------------------------
// CmdOutcome: ran vs could-not-run
// ---------------------------------------------------------------------------

#[test]
fn a_zero_exit_is_ran_and_successful() {
    let o = run("/bin/echo", &["hi"], &tmp(), QUICK);
    assert!(o.succeeded());
    assert_eq!(o.ok_stdout_trimmed().as_deref(), Some("hi"));
}

#[test]
fn a_nonzero_exit_is_ran_not_unavailable() {
    // The distinction `GhResult` could not make: this command answered. It said
    // no, but it answered.
    let o = run("/bin/sh", &["-c", "exit 4"], &tmp(), QUICK);
    assert!(matches!(o, CmdOutcome::Ran(_)), "a non-zero exit still RAN, got {o:?}");
    assert!(!o.succeeded());
    if let CmdOutcome::Ran(out) = &o {
        assert_eq!(out.status.code(), Some(4), "the exact code must survive");
    }
}

#[test]
fn a_missing_binary_is_unavailable_not_a_failed_command() {
    // `GhResult` reported this as `success: false` with the io error in stderr,
    // making "gh is not installed" identical to "gh exited 1". They are not the
    // same: one means retry elsewhere, the other means the forge said no.
    let o = run("/nonexistent/loom/not-a-real-binary", &[], &tmp(), QUICK);
    match &o {
        CmdOutcome::Unavailable(Unavailable::Spawn(_)) => {}
        other => panic!("a missing binary must be Unavailable::Spawn, got {other:?}"),
    }
    assert!(!o.succeeded());
    assert!(o.ok_output().is_none());
}

#[test]
fn a_hang_is_unavailable_with_its_partial_output() {
    let o = run("/bin/sh", &["-c", "echo partial; sleep 30"], &tmp(), Duration::from_millis(400));
    match &o {
        CmdOutcome::Unavailable(Unavailable::TimedOut { partial_stdout, .. }) => {
            assert_eq!(partial_stdout, b"partial\n", "pre-deadline output must survive");
        }
        other => panic!("expected a timeout, got {other:?}"),
    }
}

#[test]
fn failure_reason_names_which_kind_of_failure_it_was() {
    let missing = run("/nonexistent/loom/nope", &[], &tmp(), QUICK);
    let refused = run("/bin/sh", &["-c", "echo bad >&2; exit 2"], &tmp(), QUICK);

    let m = missing.failure_reason("gh pr view");
    let r = refused.failure_reason("gh pr view");

    assert!(m.contains("could not start"), "spawn failure must say so: {m}");
    assert!(r.contains("exited with"), "a non-zero exit must say so: {r}");
    assert!(r.contains("bad"), "stderr must reach the message: {r}");
    assert_ne!(m, r, "the two must not produce the same operator-facing text");
}

#[test]
fn output_is_not_trimmed_at_the_boundary() {
    // main_health_gate needed a whole separate `git_status_porcelain` helper
    // because its run_git's blanket .trim() ate the leading space of " M file".
    // Raw bytes must arrive intact so that bug cannot be reintroduced here.
    let o = run("/bin/sh", &["-c", r"printf ' M file\n'"], &tmp(), QUICK);
    let out = o.ok_output().expect("must run");
    assert_eq!(out.stdout, b" M file\n", "the leading space is load-bearing");
}

#[test]
fn non_utf8_output_is_preserved_as_bytes() {
    let o = run("/bin/sh", &["-c", r"printf 'a\377b'"], &tmp(), QUICK);
    let out = o.ok_output().expect("must run");
    assert_eq!(out.stdout, vec![b'a', 0xFF, b'b']);
}

// ---------------------------------------------------------------------------
// Query: the four meanings the old idiom merged
// ---------------------------------------------------------------------------

fn json_of(payload: &str) -> CmdOutcome {
    run("/bin/sh", &["-c", &format!("printf '%s' '{payload}'")], &tmp(), QUICK)
}

#[test]
fn a_populated_result_decodes() {
    let q: Query<Vec<Pr>> = decode_json(json_of(r#"[{"number":7,"state":"OPEN"}]"#), Vec::is_empty);
    match q {
        Query::Populated(v) => assert_eq!(
            v,
            vec![Pr {
                number: 7,
                state: "OPEN".into()
            }]
        ),
        other => panic!("expected Populated, got {other:?}"),
    }
}

#[test]
fn an_empty_array_is_empty_not_a_failure() {
    // THE regression this layer exists for. `success && !stdout.is_empty()`
    // treated "[]" as found-something (non-empty string!) or, after --jq
    // flattening, as indistinguishable from an error.
    let q: Query<Vec<Pr>> = decode_json(json_of("[]"), Vec::is_empty);
    assert!(q.is_definitely_empty(), "an empty array is a successful answer, got {q:?}");
    assert!(!q.is_unanswered(), "an empty result must not read as unanswered");
}

#[test]
fn no_output_at_all_on_a_zero_exit_is_empty_not_malformed() {
    // `gh` prints nothing for some empty queries rather than `[]`; calling that
    // a decode error would be a false alarm.
    let q: Query<Vec<Pr>> = decode_json(json_of(""), Vec::is_empty);
    assert!(q.is_definitely_empty(), "no output on a zero exit is empty, got {q:?}");
}

#[test]
fn unparseable_output_is_malformed_not_empty() {
    // Previously invisible: a `gh` that printed a warning, or a --jq filter that
    // matched nothing, both left an empty-ish string that read as "not found".
    let q: Query<Vec<Pr>> = decode_json(json_of("not json at all"), Vec::is_empty);
    match &q {
        Query::Malformed { raw, .. } => assert_eq!(raw, b"not json at all"),
        other => panic!("expected Malformed, got {other:?}"),
    }
    assert!(q.is_unanswered());
    assert!(!q.is_definitely_empty(), "malformed must never read as a definite empty");
}

#[test]
fn a_nonzero_exit_is_failed_not_empty() {
    let outcome = run("/bin/sh", &["-c", "echo 'no such PR' >&2; exit 1"], &tmp(), QUICK);
    let q: Query<Vec<Pr>> = decode_json(outcome, Vec::is_empty);
    match &q {
        Query::Failed { stderr, .. } => assert!(stderr.contains("no such PR"), "stderr: {stderr}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(!q.is_definitely_empty(), "a failure must never read as a definite empty");
    assert!(q.is_unanswered());
}

#[test]
fn an_unrunnable_query_is_unavailable_not_empty() {
    let outcome = run("/nonexistent/loom/nope", &[], &tmp(), QUICK);
    let q: Query<Vec<Pr>> = decode_json(outcome, Vec::is_empty);
    assert!(matches!(q, Query::Unavailable(_)), "got {q:?}");
    assert!(!q.is_definitely_empty(), "unknown must never read as a definite empty");
}

/// The single assertion this whole PR is for: every non-populated outcome is
/// distinguishable from a legitimate empty result.
#[test]
fn empty_is_distinguishable_from_every_way_of_not_knowing() {
    let empty: Query<Vec<Pr>> = decode_json(json_of("[]"), Vec::is_empty);
    let malformed: Query<Vec<Pr>> = decode_json(json_of("<html>"), Vec::is_empty);
    let failed: Query<Vec<Pr>> =
        decode_json(run("/bin/sh", &["-c", "exit 1"], &tmp(), QUICK), Vec::is_empty);
    let unavailable: Query<Vec<Pr>> =
        decode_json(run("/nonexistent/loom/x", &[], &tmp(), QUICK), Vec::is_empty);

    assert!(empty.is_definitely_empty());
    for (name, q) in [
        ("malformed", &malformed),
        ("failed", &failed),
        ("unavailable", &unavailable),
    ] {
        assert!(!q.is_definitely_empty(), "{name} must not read as empty");
        assert!(q.is_unanswered(), "{name} must read as unanswered");
    }
    assert!(!empty.is_unanswered(), "a real empty is answered");
}

#[test]
fn the_is_empty_predicate_is_query_specific() {
    // An empty array means "no matches". A record whose field is "" means the
    // record EXISTS and the field is unset — a different fact, and the caller
    // decides which is which.
    #[derive(Debug, Deserialize)]
    struct Field {
        value: String,
    }
    let blank: Query<Field> =
        decode_json(json_of(r#"{"value":""}"#), |f: &Field| f.value.is_empty());
    assert!(blank.is_definitely_empty(), "caller chose to treat a blank field as empty");

    let kept: Query<Field> = decode_json(json_of(r#"{"value":""}"#), |_| false);
    assert!(
        matches!(kept, Query::Populated(_)),
        "another caller may treat the same payload as populated"
    );
}

#[test]
fn value_collapses_only_where_a_caller_asks() {
    let populated: Query<Vec<Pr>> =
        decode_json(json_of(r#"[{"number":1,"state":"OPEN"}]"#), Vec::is_empty);
    assert!(populated.value().is_some());

    let empty: Query<Vec<Pr>> = decode_json(json_of("[]"), Vec::is_empty);
    assert!(empty.value().is_none(), "value() is the opt-in collapse");
}
