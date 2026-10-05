//! Static + fixture-backed contract for the story-points calibration
//! artifacts (Issue #9434, epic #9429). No Docker, network, backend or
//! credential is used, so this runs in ordinary CI on any host.
//!
//! Why this exists. The calibration loop's whole claim is that "did we get
//! the story points right?" has a standing, queryable answer whose numbers
//! mean what the questions doc says they mean. That claim is one careless
//! edit away from being false in a way nothing reports: a rubric revision
//! recorded in the doc but not in the SQL (so two populations get silently
//! averaged — the exact failure the eta-queries provenance rule exists to
//! prevent), a class median edited on one side of that mirror only, churn
//! quietly dropped from a score, or a date literal pasted into one query.
//! The static tests below pin those contracts the same way
//! [`sweep_facts_artifacts`] pins the sweep-facts seam.
//!
//! The second half is stronger than the sibling artifacts' static-only
//! checks: these queries are also EXECUTED. A fixture builds a synthetic
//! `sweep_facts` through the committed rollup INSERT
//! (`sweep-facts/sweep-facts-rollup.sql` — so the fixture payloads are
//! verified against the real extraction path, not a hand-written table),
//! then runs the committed calibration file verbatim and asserts
//! HAND-COMPUTED values: medians and quartiles, Spearman ρ including the
//! average-rank tie correction, drift ratios against the rubric's claimed
//! medians, the named misassignments, and every churn-exclusion reason.
//! The synthetic population lives ONLY here — no sample data is committed
//! into the observability artifacts themselves (honest-empty discipline:
//! the joined population is expected to be tiny or empty at first, and
//! CAL1 must report zeros, not fabricate warmth).
//!
//! One documented substitution: the fixture's `issue_landed_size` is a
//! shape-compatible view whose LSI is NULL — the committed `landed-size.sql`
//! view is fitted now (`v1-2026-10-02`, #9934), but the SQLite bundled into
//! CI lacks the math functions (`ln`, `exp`) that D1 provides and that view
//! needs. The calibration queries' own arithmetic needs none (class bounds
//! compare squares in exact integer arithmetic), so everything except the
//! LSI column is executed as committed.
#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use super::shared::{split_statements, QUERIES, QUESTION_IDS};
use std::collections::BTreeMap;

use regex::Regex;

const QUESTIONS: &str =
    include_str!("../../../defaults/observability/story-points-calibration-questions.md");
const RUBRIC_DOC: &str = include_str!("../../../defaults/docs/story-points.md");
const LANDED_SIZE: &str =
    include_str!("../../../defaults/observability/sweep-facts/landed-size.sql");

/// The measured point ratios from the story-points experiment (#9466) — the
/// one place they may be written down is `landed-size.sql`'s
/// `measured_point_values` view. The calibration file must not restate them
/// (a second copy is a second meaning of "a point", and the throughput side
/// that owns them is #9433's, not this artifact's).
const MEASURED_POINT_RATIOS: &[&str] = &[
    "1.3", "2.2", "3.4", "5.1", "8.2", "8.5", "21.0", "47.0", "82.0", "197.0",
];

// ---------------------------------------------------------------------------
// Static helpers (the sweep-facts/cycle-time contract-test vocabulary)
// ---------------------------------------------------------------------------

/// Only the EXECUTABLE lines of a SQL artifact — `--` comment lines dropped.
/// These files document their own pitfalls in prose, so a check for a
/// forbidden token has to distinguish naming it from using it.
fn non_comment_lines(sql: &str) -> String {
    sql.lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One CAL question's query body, cut at the next `-- CALn. ` marker.
fn question_body(id: &str) -> &str {
    let start = QUERIES
        .find(&format!("-- {id}. "))
        .unwrap_or_else(|| panic!("story-points-calibration-queries.sql implements no {id}"));
    let end = QUESTION_IDS
        .iter()
        .filter_map(|next| QUERIES[start + 1..].find(&format!("-- {next}. ")))
        .min()
        .map(|offset| start + 1 + offset)
        .unwrap_or(QUERIES.len());
    &QUERIES[start..end]
}

/// The landing predicate as written in `sql` after the `anchor` marker,
/// normalized: table aliases stripped and whitespace collapsed, so the same
/// predicate spelled with a different alias or wrapped at a different column
/// still compares equal. The anchor exists because these files also
/// *describe* the predicate in prose comments.
fn normalized_landing_predicate(sql: &str, anchor: &str) -> String {
    const OPEN: &str = "disposition = 'landed'";
    const CLOSE: &str = "pr_number IS NOT NULL";
    let at = sql
        .find(anchor)
        .unwrap_or_else(|| panic!("no '{anchor}' anchor to read a landing predicate after"));
    let start = sql[at..]
        .find(OPEN)
        .unwrap_or_else(|| panic!("no landing predicate after '{anchor}'"))
        + at;
    let end = sql[start..]
        .find(CLOSE)
        .unwrap_or_else(|| panic!("landing predicate after '{anchor}' has no pre-#9441 fallback"))
        + start
        + CLOSE.len();
    let de_aliased = Regex::new(r"\b[a-z]\.")
        .unwrap()
        .replace_all(&sql[start..end], "");
    Regex::new(r"\s+")
        .unwrap()
        .replace_all(&de_aliased, " ")
        .into_owned()
}

/// The body of a `CREATE VIEW IF NOT EXISTS <name>` block, up to its
/// terminating semicolon (statement-aware, so semicolons quoted in the
/// block's comments do not cut it short).
fn view_block<'a>(sql: &'a str, name: &str) -> &'a str {
    let header = format!("CREATE VIEW IF NOT EXISTS {name}");
    let start = sql
        .find(&header)
        .unwrap_or_else(|| panic!("no {header} block"));
    let statements = split_statements(&sql[start..]);
    let (_, first) = statements.first().expect("view block is never terminated");
    &sql[start..start + first.trim_end().len()]
}

/// The revision markers written into the `rubric_revisions` view.
fn sql_revision_markers() -> Vec<String> {
    let row = Regex::new(r"SELECT '(v\d+)'").unwrap();
    view_block(QUERIES, "rubric_revisions")
        .split(';')
        .filter_map(|part| row.captures(part).map(|capture| capture[1].to_owned()))
        .collect()
}

/// The revision markers recorded in the rubric doc's history table.
fn doc_revision_markers() -> Vec<String> {
    let history = RUBRIC_DOC
        .split("## Revision history")
        .nth(1)
        .expect("story-points.md has no '## Revision history' section (#9434)");
    Regex::new(r"(?m)^\| (v\d+) \|")
        .unwrap()
        .captures_iter(history)
        .map(|capture| capture[1].to_owned())
        .collect()
}

/// The rubric's class table as claimed in the SQL mirror:
/// class -> (hw_lines_median, hw_files_median, tokens_median).
fn sql_rubric_classes() -> BTreeMap<String, (u64, u64, u64)> {
    let row = Regex::new(r"(?m)^\s*\('(?:v\d+)', '(\d+)',\s*([\d,]+),\s*(\d+),\s*(\d+)\)[,;]?\s*$")
        .unwrap();
    row.captures_iter(view_block(QUERIES, "rubric_classes"))
        .map(|capture| {
            (
                capture[1].to_owned(),
                (
                    capture[2].replace(',', "").parse().unwrap(),
                    capture[3].parse().unwrap(),
                    capture[4].parse().unwrap(),
                ),
            )
        })
        .collect()
}

/// The rubric's class table as published in the doc (`~12` / `1` / `~14M`).
fn doc_rubric_classes() -> BTreeMap<String, (u64, u64, u64)> {
    let row = Regex::new(r"(?m)^\| (\d+) \| ~([\d,]+) \| (\d+)\+? \| ~(\d+)M \|$").unwrap();
    row.captures_iter(RUBRIC_DOC)
        .map(|capture| {
            (
                capture[1].to_owned(),
                (
                    capture[2].replace(',', "").parse().unwrap(),
                    capture[3].parse().unwrap(),
                    capture[4].parse::<u64>().unwrap() * 1_000_000,
                ),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Static contracts
// ---------------------------------------------------------------------------

#[test]
fn the_documented_question_set_is_the_implemented_one() {
    for id in QUESTION_IDS {
        assert!(
            QUESTIONS.contains(&format!("**{id}**")),
            "story-points-calibration-questions.md documents no {id}"
        );
        let implementations = QUERIES.matches(&format!("-- {id}. ")).count();
        assert_eq!(
            implementations, 1,
            "story-points-calibration-queries.sql implements {id} {implementations} times; \
             expected exactly once"
        );
    }
    let stray = Regex::new(r"(?m)^-- (CAL\d+)\. ").unwrap();
    for capture in stray.captures_iter(QUERIES) {
        assert!(
            QUESTION_IDS.contains(&&capture[1]),
            "story-points-calibration-queries.sql implements {}, which is not in the canonical \
             question set",
            &capture[1]
        );
    }
}

#[test]
fn the_window_is_bound_once_and_no_question_carries_a_date_literal() {
    // The D1 adaptation of the cycle-time binding rule (no client-side bind
    // parameters for a committed file): the window lives in the `spc_window`
    // view — the one place a window date literal may appear — and every
    // question reads derived views built on it instead of carrying its own
    // literal. The rubric_revisions dates are history markers, not a window,
    // and live in their own block before CAL1.
    assert_eq!(
        QUERIES
            .matches("CREATE VIEW IF NOT EXISTS spc_window")
            .count(),
        1,
        "the calibration file must define exactly one spc_window view; per-query windows are \
         the hand-edited-literal habit these artifacts replace"
    );
    let date_literal = Regex::new(r"(?m)^[^-].*'20\d\d-\d\d-\d\d").unwrap();
    for id in QUESTION_IDS {
        let body = question_body(id);
        assert!(
            body.contains("spc_scored") || body.contains("spc_landings"),
            "{id} reads neither spc_scored nor spc_landings, so it has escaped the bound \
             spc_window; editing a date literal into the SQL is the habit these artifacts \
             replace"
        );
        assert!(
            date_literal.find(body).is_none(),
            "a date literal was written into {id} outside a comment"
        );
    }
}

#[test]
fn the_landing_predicate_mirrors_the_shared_one() {
    // The calibration must score the same population the rubric's evidence
    // (story-points-queries.sql) and the landed-size index count: the
    // predicate is not merely present, it is the SAME predicate
    // landed-size.sql uses, normalized so alias/wrapping differences cannot
    // hide a divergence.
    let spc = normalized_landing_predicate(QUERIES, "CREATE VIEW IF NOT EXISTS spc_landings");
    let landed_size = normalized_landing_predicate(LANDED_SIZE, "landings AS (");
    assert_eq!(
        spc, landed_size,
        "spc_landings' landing predicate and landed-size.sql's `landings` CTE disagree; the \
         calibration would score a different population than the one the rubric was derived \
         from (#9434)"
    );
    assert!(
        landed_size.contains("disposition IS NULL"),
        "the shared landing predicate lost its documented pre-#9441 fallback"
    );
}

#[test]
fn every_calibration_view_separates_its_populations_by_rubric_revision() {
    // The eta-queries provenance rule (#9289), restated for the estimator
    // that changes: every view groups by the rubric revision a landing was
    // sized under, so a rubric change mid-window shows up as two
    // populations, never a silent average.
    for id in QUESTION_IDS {
        assert!(
            question_body(id).contains("rubric_revision"),
            "{id} does not carry the rubric revision, so it would silently average across a \
             rubric change — the exact failure the provenance rule exists to prevent (#9434)"
        );
    }
    assert!(
        view_block(QUERIES, "spc_landings").contains("FROM rubric_revisions"),
        "spc_landings no longer resolves its revision marker from the rubric_revisions view, \
         so the marker the views group by has no source"
    );
    assert!(
        QUESTIONS.contains("two populations"),
        "the questions doc no longer states the two-populations provenance rule the views \
         implement"
    );
}

#[test]
fn the_excluded_population_is_counted_beside_every_score() {
    // Churn (repair loops, re-judges) is excluded from the SCORE because it
    // is process cost, not estimate error — but excluded must never mean
    // dropped: every view that reports a figure carries the excluded count
    // beside it, and CAL6 is the reason-level breakdown.
    assert!(
        question_body("CAL2").contains("excluded_churn"),
        "CAL2 reports no per-bucket excluded_churn beside its distributions (#9434's churn \
         accounting)"
    );
    for id in ["CAL3", "CAL5"] {
        assert!(
            question_body(id).contains("excluded_from_score"),
            "{id} reports no excluded_from_score beside its figures (#9434's churn accounting)"
        );
    }
    assert!(
        question_body("CAL4").contains("excluded_churn"),
        "CAL4 reports no per-bucket excluded_churn beside its drift verdicts"
    );
    assert!(
        question_body("CAL6").contains("churn_class"),
        "CAL6 no longer breaks the excluded population down by churn_class"
    );
    let cal1 = question_body("CAL1");
    for reason in [
        "excl_no_phase_data",
        "excl_no_judge_phase",
        "excl_rejudge_loop",
        "excl_doctor_repair",
        "excl_doctor_cycles_only",
    ] {
        assert!(
            cal1.contains(reason),
            "CAL1 no longer counts the '{reason}' exclusion; a population the clean-landing \
             filter drops would become invisible (#9434)"
        );
    }
    let spc_landings = view_block(QUERIES, "spc_landings");
    for reason in [
        "no_phase_data",
        "no_judge_phase",
        "rejudge_loop",
        "doctor_repair",
        "doctor_cycles_only",
    ] {
        assert!(
            spc_landings.contains(&format!("'{reason}'")),
            "spc_landings no longer classifies the '{reason}' churn reason"
        );
    }
}

#[test]
fn the_rubric_revision_markers_match_the_documented_history() {
    let sql = sql_revision_markers();
    let doc = doc_revision_markers();
    assert!(
        !sql.is_empty() && !doc.is_empty(),
        "failed to parse revision markers from the SQL mirror and/or the doc history"
    );
    assert_eq!(
        sql, doc,
        "the rubric_revisions view and story-points.md's '## Revision history' name \
         different revision markers; the calibration would group by a revision the doc does \
         not record (or vice versa) — update both together (#9434)"
    );
    let history = RUBRIC_DOC
        .split("## Revision history")
        .nth(1)
        .expect("story-points.md has no '## Revision history' section");
    assert!(
        history.contains("CAL4"),
        "the revision history does not reference the calibration view (CAL4) that triggers a \
         revision; the doc should say updating the rubric from calibration output is normal"
    );
    assert!(
        history.contains("normal, expected change"),
        "the rubric doc no longer states that revising from calibration output is a normal, \
         expected change — the issue's revision-path deliverable"
    );
    assert!(
        history.contains("#9520"),
        "the revision history does not cite #9520 for v1, the revision that landed the rubric"
    );
}

#[test]
fn the_rubric_class_table_mirrors_the_documented_rubric() {
    let sql = sql_rubric_classes();
    let doc = doc_rubric_classes();
    let mut vocabulary: Vec<&str> = sql.keys().map(String::as_str).collect();
    vocabulary.sort_unstable();
    assert_eq!(
        vocabulary,
        ["1", "13", "2", "3", "5", "8"],
        "the SQL rubric_classes view no longer carries the closed points vocabulary"
    );
    assert_eq!(
        sql, doc,
        "the rubric_classes view and story-points.md's class table disagree; the calibration \
         would score estimates against a claim the published rubric does not make — the doc \
         is authoritative, update the mirror (the contract test exists so this cannot go \
         stale silently)"
    );
}

#[test]
fn ordinal_labels_are_never_summed_and_experiment_ratios_not_restated() {
    // Fibonacci labels are ORDINAL (#9429/#9466): a bare sum over
    // story_points is not a size, and the experiment's measured bucket
    // ratios belong to landed-size.sql's measured_point_values view alone
    // (the throughput side, #9433 — restating them here would also blur the
    // calibration/throughput scope boundary).
    let bare_sum = Regex::new(r"(?m)^.*\bsum\(\s*(?:[a-z0-9_]+\.)?story_points\s*\).*$").unwrap();
    assert!(
        bare_sum.find_iter(QUERIES).next().is_none(),
        "a calibration query sums the raw story_points column; Fibonacci labels are ordinal, \
         never a size"
    );
    for line in non_comment_lines(QUERIES).lines() {
        for ratio in MEASURED_POINT_RATIOS {
            assert!(
                !line.contains(ratio),
                "executable SQL restates the measured bucket ratio {ratio}:\n  {}\nRead it \
                 from landed-size.sql's measured_point_values view instead — a second copy is \
                 a second meaning of \"a point\"",
                line.trim()
            );
        }
    }
    assert!(
        !QUERIES.contains("measured_point_values"),
        "the calibration file joins measured_point_values; that view is the throughput side's \
         (#9433, SF8) and this artifact is the calibration side — keep the two disjoint"
    );
}

// ---------------------------------------------------------------------------
// Fixture execution: the committed chain, run verbatim
