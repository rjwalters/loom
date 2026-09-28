//! Static contract for the PR-latency question set (Issue #8923). No Docker,
//! network, forge or credential is used, so this runs in ordinary CI on any
//! host.
//!
//! Why this exists. `pr-latency`'s whole claim is that a figure quoted from it
//! means the same thing wherever it is quoted, because the questions are
//! defined once in `defaults/docs/pr-latency.md` and implemented once in
//! `loom-daemon/src/pr_latency/`. That claim is one careless edit away from
//! being false in a way nothing reports: a question documented in prose that
//! nothing implements, a renderer that stops emitting a question's row, or —
//! worst and most likely — the *dwell vs. age* and *absent vs. zero* rules
//! quietly decaying out of the doc that exists to pin them.
//!
//! Follows the shape of [`cycle_time_artifacts`] (#8665): the authorities are
//! the committed artifacts themselves, compared against one pinned set stated
//! here, so that two files cannot drift *together* into agreeing on the wrong
//! thing.
#![allow(clippy::unwrap_used)]

const QUESTIONS: &str = include_str!("../../defaults/docs/pr-latency.md");
const REPORT: &str = include_str!("../src/pr_latency/report.rs");
const SEGMENTS: &str = include_str!("../src/pr_latency/segments.rs");
const STATS: &str = include_str!("../src/pr_latency/stats.rs");
const RENDER: &str = include_str!("../src/cli/pr_latency_render.rs");

/// The canonical question IDs. Pinned here deliberately: this is the one place
/// the *set* is fixed, and every artifact below is checked against it rather
/// than against another artifact.
const QUESTION_IDS: &[&str] = &["PL1", "PL2", "PL3a", "PL3b", "PL4", "PL5a", "PL5b", "PL6"];

#[test]
fn every_question_is_documented() {
    for id in QUESTION_IDS {
        assert!(
            QUESTIONS.contains(&format!("**{id}**")),
            "{id} is implemented but the question set does not define it; \
             add a row to defaults/docs/pr-latency.md"
        );
    }
}

#[test]
fn every_question_is_implemented() {
    // Each ID must appear as a `**PLn**` marker on the struct field or
    // derivation that answers it, so a question cannot be documented in prose
    // with nothing behind it.
    let code = format!("{REPORT}{SEGMENTS}");
    for id in QUESTION_IDS {
        assert!(
            code.contains(&format!("**{id}**")),
            "{id} is documented but no field in report.rs/segments.rs claims it"
        );
    }
}

#[test]
fn the_documented_set_has_no_extra_ids() {
    // Catches a question added to the doc under a new ID without being pinned
    // in QUESTION_IDS — which would otherwise be invisible to both tests above.
    let mut found: Vec<String> = Vec::new();
    for line in QUESTIONS.lines() {
        let mut rest = line;
        while let Some(i) = rest.find("**PL") {
            rest = &rest[i + 2..];
            if let Some(end) = rest.find("**") {
                found.push(rest[..end].to_string());
                rest = &rest[end..];
            } else {
                break;
            }
        }
    }
    for id in &found {
        assert!(
            QUESTION_IDS.contains(&id.as_str()),
            "the question set documents {id}, which is not pinned in QUESTION_IDS"
        );
    }
    assert!(!found.is_empty(), "no question IDs found in the doc at all");
}

#[test]
fn the_renderer_emits_every_question() {
    // A question that is derived but never printed is not measurable "on
    // demand" in the sense acceptance criterion 1 asks for.
    for id in QUESTION_IDS {
        assert!(
            RENDER.contains(id),
            "{id} is never rendered, so it cannot be read off the report"
        );
    }
}

#[test]
fn the_gate_split_is_present_on_both_sides() {
    // Acceptance criterion 2: the loom:pr -> merged segment must be reported
    // SPLIT by operator-gate presence. Two distributions, not one plus a flag.
    assert!(REPORT.contains("merge_wait_gated"));
    assert!(REPORT.contains("merge_wait_ungated"));
    assert!(RENDER.contains("OPERATOR-GATED"));
    assert!(
        QUESTIONS.contains("**PL3a**") && QUESTIONS.contains("**PL3b**"),
        "the doc must define the split as two questions, not describe one"
    );
}

#[test]
fn the_doc_pins_dwell_versus_age() {
    // The conflation this whole issue exists to correct. If this rule ever
    // leaves the doc, the next reader has nothing telling them the queue
    // numbers are not PR ages.
    assert!(
        QUESTIONS.contains("Dwell is not age"),
        "the doc must keep the dwell-vs-age section that explains the #8923 correction"
    );
    assert!(
        SEGMENTS.contains("**This is dwell, not age.**"),
        "the dwell_secs field must keep stating that it is dwell, not age"
    );
    // Both must be reported, so the two can never be quoted as one.
    assert!(SEGMENTS.contains("dwell_secs") && SEGMENTS.contains("age_secs"));
}

#[test]
fn the_doc_pins_absent_versus_zero() {
    assert!(
        QUESTIONS.contains("**absent vs. zero**"),
        "the doc must keep the absent-vs-zero definition"
    );
    // And the implementation must actually return an absence: a Distribution
    // over no samples has no percentiles.
    assert!(
        STATS.contains("Option<i64>"),
        "stats.rs must keep percentiles optional so 'unmeasured' cannot print as 0"
    );
}

#[test]
fn the_doc_records_why_this_is_not_the_cycle_time_rollup() {
    // The design decision most likely to be re-litigated by a later reader who
    // finds two latency artifacts and assumes one is redundant.
    assert!(QUESTIONS.contains("cycle-time-questions.md"));
    assert!(
        QUESTIONS.contains("no durable label-transition store")
            || QUESTIONS.contains("**no** durable label-transition store"),
        "the doc must state why the figures are derived live"
    );
}

#[test]
fn the_advisory_populations_are_documented_as_disjoint() {
    // The advisory's total is a count of PRs, not of findings — which is only
    // true while the four populations stay mutually exclusive.
    for phrase in [
        "Waiting on a person",
        "Approved, nothing holding it",
        "Awaiting a Judge verdict",
        "Rejected, no Doctor push",
    ] {
        assert!(
            QUESTIONS.contains(phrase),
            "the advisory population {phrase:?} is not documented"
        );
    }
    assert!(
        QUESTIONS.contains("disjoint"),
        "the doc must state that the advisory populations are disjoint"
    );
    // The implementation's exclusions that make them disjoint.
    assert!(REPORT.contains("!r.parked && !r.operator_gated"));
}

#[test]
fn the_advisory_contract_matches_its_siblings() {
    // Advisory-only: always exit 0, read-only, never writes a label.
    assert!(QUESTIONS.contains("always exits 0"));
    assert!(
        QUESTIONS.contains("never writes a label"),
        "the doc must state the read-only contract shared with the pre-wave checks"
    );
    assert!(
        RENDER.contains("the hold is legitimate; the silence is not"),
        "the advisory must keep saying that the gate is not the defect"
    );
}

#[test]
fn the_committed_date_caveat_is_disclosed() {
    // PL4 reads low for a human who commits locally and pushes later. An
    // undisclosed systematic bias is worse than a disclosed one.
    assert!(
        QUESTIONS.contains("committed") && QUESTIONS.contains("head_ref_force_pushed"),
        "the doc must disclose that a `committed` date is not a push time"
    );
}
