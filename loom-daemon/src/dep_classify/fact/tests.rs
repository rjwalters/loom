//! Tests for the `--check-fact-unescalate` decision (epic #7810, PR 3).

use super::*;

const OPERATOR_ONLY: &str = "loom:operator-only";
const CYCLE_PREFIX: &str = "<!-- champion:dependency-cycle ";
const FACT_PREFIX: &str = "<!-- champion:fact-unescalated:";

fn markers() -> Markers<'static> {
    Markers {
        operator_only_label: OPERATOR_ONLY,
        cycle_prefix: CYCLE_PREFIX,
        fact_unescalate_prefix: FACT_PREFIX,
    }
}

/// A parked issue with two findings, both resolved, and a commit.
fn ready() -> Inputs {
    Inputs {
        labels: vec![OPERATOR_ONLY.to_string()],
        comments: String::new(),
        escalation: "- first finding\n- second finding\n".to_string(),
        resolutions: Some("RESOLVED: first\nRESOLVED: second\n".to_string()),
        commit_sha: Some("abc1234".to_string()),
    }
}

fn decide_it(i: &Inputs) -> Decision {
    decide(i, &markers())
}

fn no(reason: &'static str) -> Decision {
    Decision::NoFactUnescalate { reason }
}

#[test]
fn a_fully_resolved_set_with_a_commit_releases() {
    match decide_it(&ready()) {
        Decision::FactUnescalate {
            verified_commit,
            resolved_count,
            fingerprint,
        } => {
            assert_eq!(verified_commit, "abc1234");
            assert_eq!(resolved_count, 2);
            assert!(fingerprint.starts_with("fact-"), "{fingerprint}");
        }
        other => panic!("expected FactUnescalate, got {other:?}"),
    }
}

#[test]
fn an_unparked_issue_is_refused() {
    let mut i = ready();
    i.labels.clear();
    assert_eq!(decide_it(&i), no("not-operator-only"));
}

#[test]
fn a_cycle_escalation_is_refused() {
    let mut i = ready();
    i.comments = format!("{CYCLE_PREFIX}abc -->");
    assert_eq!(decide_it(&i), no("cycle-escalation"));
}

#[test]
fn a_missing_resolutions_file_is_refused() {
    let mut i = ready();
    i.resolutions = None;
    assert_eq!(decide_it(&i), no("missing-resolutions-file"));
}

/// Half the counting gate: FEWER resolutions than findings.
#[test]
fn resolutions_covering_only_some_findings_are_refused() {
    let mut i = ready();
    i.resolutions = Some("RESOLVED: first\n".into());
    assert_eq!(
        decide_it(&i),
        no("resolutions-mismatch"),
        "a file covering one of two findings must not release the issue"
    );
}

/// The other half: MORE resolutions than findings. Equality, not "at least".
#[test]
fn more_resolutions_than_findings_are_refused() {
    let mut i = ready();
    i.resolutions = Some("RESOLVED: a\nRESOLVED: b\nRESOLVED: c\n".into());
    assert_eq!(
        decide_it(&i),
        no("resolutions-mismatch"),
        "a file describing more than this issue's findings is not evidence about it"
    );
}

/// The gate the equality check cannot catch on its own.
#[test]
fn a_matching_count_of_unresolved_lines_is_still_refused() {
    let mut i = ready();
    i.resolutions = Some("UNRESOLVED: first\nUNRESOLVED: second\n".into());
    assert_eq!(
        decide_it(&i),
        no("partial-resolution"),
        "counts match, but nothing was actually resolved"
    );
}

#[test]
fn one_unresolved_line_among_resolved_ones_is_refused() {
    let mut i = ready();
    i.resolutions = Some("RESOLVED: first\nUNRESOLVED: second\n".into());
    assert_eq!(decide_it(&i), no("partial-resolution"));
}

#[test]
fn prose_mentioning_the_tokens_mid_line_does_not_count() {
    // Only line-leading tokens count (`grep -cE '^(RESOLVED|UNRESOLVED):'`).
    // Otherwise a narrative file could satisfy the equality gate by accident.
    let mut i = ready();
    i.resolutions =
        Some("RESOLVED: first\nthis line says RESOLVED: but is prose\nRESOLVED: second\n".into());
    assert!(
        matches!(decide_it(&i), Decision::FactUnescalate { .. }),
        "mid-line mentions must not inflate the count"
    );
}

#[test]
fn a_missing_commit_is_refused() {
    let mut i = ready();
    i.commit_sha = None;
    assert_eq!(decide_it(&i), no("missing-commit"));

    let mut empty = ready();
    empty.commit_sha = Some(String::new());
    assert_eq!(decide_it(&empty), no("missing-commit"));
}

#[test]
fn an_existing_marker_short_circuits() {
    let i = ready();
    let Decision::FactUnescalate { fingerprint, .. } = decide_it(&i) else {
        panic!("expected a release");
    };
    let mut again = ready();
    again.comments = format!("{FACT_PREFIX}{fingerprint} -->");
    assert_eq!(decide_it(&again), no("already-unescalated"));
}

#[test]
fn a_different_commit_produces_a_different_fingerprint() {
    // Releasing against one commit must not suppress a later release against
    // another — the evidence is different.
    let a = ready();
    let mut b = ready();
    b.commit_sha = Some("def5678".into());

    let (
        Decision::FactUnescalate {
            fingerprint: fa, ..
        },
        Decision::FactUnescalate {
            fingerprint: fb, ..
        },
    ) = (decide_it(&a), decide_it(&b))
    else {
        panic!("both should release");
    };
    assert_ne!(fa, fb);
}

#[test]
fn an_escalation_with_no_bullets_is_refused() {
    let mut i = ready();
    i.escalation = "Just prose, no findings list.\n".into();
    assert_eq!(decide_it(&i), no("no-findings"));
}

#[test]
fn an_empty_escalation_is_refused() {
    let mut i = ready();
    i.escalation = "   \n".into();
    assert_eq!(decide_it(&i), no("no-escalation-record"));
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[test]
fn a_release_renders_its_evidence() {
    let (out, code) = render(&Decision::FactUnescalate {
        verified_commit: "abc1234".into(),
        resolved_count: 2,
        fingerprint: "fact-xyz".into(),
    });
    assert_eq!(
        out,
        "FACT_UNESCALATE\nVERIFIED_COMMIT: abc1234\nRESOLVED_COUNT: 2\nFINGERPRINT: fact-xyz\n"
    );
    assert_eq!(code, 0);
}

#[test]
fn a_refusal_renders_its_reason_and_exits_one() {
    let (out, code) = render(&no("partial-resolution"));
    assert_eq!(out, "NO_FACT_UNESCALATE\nREASON: partial-resolution\n");
    assert_eq!(code, 1);
}
