//! Static contract for the story-point calibration artifacts (Issue #9430). No
//! Docker, network, backend or credential is used, so this runs in ordinary CI
//! on any host.
//!
//! Why this exists. The rubric in `defaults/docs/story-points.md` claims its
//! bucket bounds are derived from measured clean-landing distributions. That
//! claim is one careless edit away from being false in a way nothing reports:
//! an extraction view reading an attribute key the gateway's `keep_keys`
//! allowlist strips (which yields NULL forever, silently — the exact trap
//! `loom.disposition` sets for this question set, since #9441's nicer landed
//! test does not survive the gateway), a column renamed on one backend only,
//! the clean-landing filter re-derived differently in two queries, or a
//! hand-edited date literal replacing the bound window.
//!
//! Follows the shape of [`cycle_time_artifacts`] (#8665): the authorities are
//! derived from the committed artifacts themselves and compared against one
//! pinned set stated here, so two files cannot drift *together* into agreeing
//! on the wrong thing.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;

use regex::Regex;

const QUESTIONS: &str = include_str!("../../defaults/observability/story-point-questions.md");
const QUERIES: &str = include_str!("../../defaults/observability/story-point-queries.sql");
const CLICKSTACK_EXTRACT: &str =
    include_str!("../../defaults/observability/clickstack/story-point-extract.sql");
const SIGNOZ_EXTRACT: &str =
    include_str!("../../defaults/observability/signoz/story-point-extract.sql");
const COLLECTOR_CONFIG: &str = include_str!("../../defaults/observability/collector/config.yaml");
const RUBRIC: &str = include_str!("../../defaults/docs/story-points.md");

/// The canonical question IDs. Restated here deliberately: this is the one
/// place the *set* is pinned, and every artifact below is checked against it
/// rather than against another artifact, so two files cannot drift together.
const QUESTION_IDS: &[&str] = &["SP1", "SP2", "SP3", "SP4", "SP5"];

/// The questions whose whole point is the clean subset. SP3 deliberately
/// reads the full population — it is the exclusion accounting — so it is not
/// required to filter, only to reference the verdict.
const CLEAN_SUBSET_QUESTIONS: &[&str] = &["SP1", "SP2", "SP4", "SP5"];

/// The token cut parameters SP4 and SP5 bind, in order. The rubric's bounds
/// and the live calibration run both flow through these, so a rename here
/// must fail loudly instead of silently defaulting a cut to zero.
const CUT_PARAMS: &[&str] = &[
    "cut_1_2",
    "cut_2_3",
    "cut_3_5",
    "cut_5_8",
    "cut_8_13",
];

/// Output aliases of an extraction view: every `… AS name` at the end of a line
/// between the top-level `SELECT` and its `FROM`. Order is preserved because a
/// silent transposition is exactly what an unordered comparison would miss.
fn view_output_columns(sql: &str) -> Vec<String> {
    let select = sql
        .find("\nSELECT\n")
        .expect("extraction view has no top-level SELECT");
    let from = sql[select..]
        .find("\nFROM ")
        .expect("extraction view has no FROM")
        + select;
    Regex::new(r"(?m)\bAS\s+([a-z_][a-z0-9_]*)\s*,?\s*$")
        .unwrap()
        .captures_iter(&sql[select..from])
        .map(|capture| capture[1].to_owned())
        .collect()
}

/// Keys read out of a String-valued attribute/resource map, in any of the
/// backends' container column names. A key read for its value must survive the
/// gateway, or the query silently returns nothing.
fn referenced_keys(sql: &str, containers: &[&str]) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for container in containers {
        for pattern in [
            format!(r"{container}\['([^']+)'\]"),
            format!(r"mapContains\(\s*{container}\s*,\s*'([^']+)'\s*\)"),
        ] {
            for capture in Regex::new(&pattern).unwrap().captures_iter(sql) {
                keys.insert(capture[1].to_owned());
            }
        }
    }
    keys
}

/// The gateway's `keep_keys` allowlist for one OTTL context, parsed from the
/// collector config the deployment mounts.
fn allowlist(context: &str) -> BTreeSet<String> {
    let quoted = Regex::new(r#""([^"]+)""#).unwrap();
    let mut current = String::new();
    let mut keys = BTreeSet::new();
    for line in COLLECTOR_CONFIG.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- context:") {
            current = rest.trim().to_owned();
        }
        if current == context && trimmed.contains("keep_keys(") {
            for capture in quoted.captures_iter(trimmed) {
                keys.insert(capture[1].to_owned());
            }
        }
    }
    assert!(!keys.is_empty(), "failed to parse the '{context}' keep_keys allowlist");
    keys
}

/// One SP question's query body, cut at the next `-- SPn. ` marker.
fn question_body(id: &str) -> &str {
    let start = QUERIES
        .find(&format!("-- {id}. "))
        .unwrap_or_else(|| panic!("story-point-queries.sql implements no {id}"));
    let end = QUESTION_IDS
        .iter()
        .filter_map(|next| QUERIES[start + 1..].find(&format!("-- {next}. ")))
        .min()
        .map(|offset| start + 1 + offset)
        .unwrap_or(QUERIES.len());
    &QUERIES[start..end]
}

#[test]
fn both_backends_extract_the_same_normalized_landing_cost_columns() {
    let clickstack = view_output_columns(CLICKSTACK_EXTRACT);
    let signoz = view_output_columns(SIGNOZ_EXTRACT);
    assert!(
        !clickstack.is_empty(),
        "parsed no output columns from the ClickStack extraction view"
    );
    assert!(
        clickstack.contains(&"clean_landing".to_owned()),
        "the ClickStack extraction view no longer exposes the clean_landing verdict"
    );
    assert_eq!(
        clickstack, signoz,
        "the ClickStack and SigNoz extraction views no longer expose the same landing-cost \
         columns in the same order, so the shared question set cannot read both and \
         cross-backend parity is broken (#8529)"
    );
}

#[test]
fn every_attribute_key_the_extractions_read_survives_the_gateway() {
    let log_keys = allowlist("log");
    for (label, sql, containers) in [
        ("clickstack/story-point-extract.sql", CLICKSTACK_EXTRACT, vec!["LogAttributes"]),
        ("signoz/story-point-extract.sql", SIGNOZ_EXTRACT, vec!["attributes_string", "attributes_number"]),
    ] {
        let read = referenced_keys(sql, &containers);
        assert!(
            read.iter().any(|key| key == "loom.phase_durations"),
            "{label} no longer reads loom.phase_durations, the judge-count half of the \
             clean-landing filter"
        );
        assert!(
            read.iter().any(|key| key == "loom.tokens_in"),
            "{label} no longer reads loom.tokens_in, the primary anchor"
        );
        for key in read {
            assert!(
                log_keys.contains(&key),
                "{label} reads attribute '{key}', which the gateway's keep_keys allowlist \
                 strips — the view can only ever read NULL for it, silently, which is \
                 indistinguishable from the fleet having landed nothing"
            );
        }
    }
}

#[test]
fn the_documented_question_set_is_the_implemented_one() {
    for id in QUESTION_IDS {
        assert!(
            QUESTIONS.contains(&format!("**{id}**")),
            "story-point-questions.md documents no {id}"
        );
        let implementations = QUERIES.matches(&format!("-- {id}. ")).count();
        assert_eq!(
            implementations, 1,
            "story-point-queries.sql implements {id} {implementations} times; expected exactly once"
        );
    }
    let stray = Regex::new(r"(?m)^-- (SP\d+)\. ").unwrap();
    for capture in stray.captures_iter(QUERIES) {
        assert!(
            QUESTION_IDS.contains(&&capture[1]),
            "story-point-queries.sql implements {}, which is not in the canonical question set",
            &capture[1]
        );
    }
}

#[test]
fn every_question_binds_its_window_instead_of_hardcoding_one() {
    for id in QUESTION_IDS {
        assert!(
            question_body(id).contains("{since:DateTime}"),
            "{id} does not bind its window as a parameter; editing a date literal into the SQL \
             is the hand-written-SQL habit these artifacts replace"
        );
    }
    let date_literal = Regex::new(r"(?m)^[^-].*'20\d\d-\d\d-\d\d").unwrap();
    assert!(
        date_literal.find(QUERIES).is_none(),
        "a date literal was written into a story-point query outside a comment"
    );
}

#[test]
fn queries_are_backend_neutral_and_read_only_the_seam() {
    // The question set's whole parity claim is that it reads exactly one
    // object. A query reaching into a raw backend table would fork the
    // definition of a clean landing across backends.
    for raw in ["signoz_logs.", "otel_logs", "distributed_logs_v2"] {
        assert!(
            !QUERIES.contains(raw),
            "a story-point query reads the raw backend table '{raw}'; read \
             loom_analytics.raw_landing_cost so both backends answer from one definition"
        );
    }
    let stray_from = Regex::new(r"(?m)^\s*FROM\s+(\S+)").unwrap();
    let mut froms = 0;
    for capture in stray_from.captures_iter(QUERIES) {
        froms += 1;
        assert_eq!(
            capture[1].trim_end_matches(';'),
            "loom_analytics.raw_landing_cost",
            "a story-point query reads something other than the seam view; read \
             loom_analytics.raw_landing_cost so both backends answer from one definition"
        );
    }
    assert!(
        froms >= QUESTION_IDS.len(),
        "parsed suspiciously few FROM clauses ({froms}); the question set seems malformed"
    );
}

#[test]
fn the_clean_landing_filter_is_defined_once_in_the_views() {
    // The filter lives in the extraction views as `clean_landing`; a query
    // that re-derives it from components can drift from the views' verdict.
    for id in CLEAN_SUBSET_QUESTIONS {
        assert!(
            question_body(id).contains("clean_landing = 1"),
            "{id} does not select the clean subset via the view's clean_landing verdict; \
             re-deriving the filter in SQL is how two clean-landing populations drift apart"
        );
    }
    assert!(
        question_body("SP3").contains("clean_landing"),
        "SP3 no longer references the clean_landing verdict for its exclusion accounting"
    );
    // SP3 may reference the view's derived columns inside countIf to COUNT
    // an exclusion; what no query may do is rebuild the filter's judge/doctor
    // machinery from the raw phase array — that is how a second, drifting
    // clean-landing population would start existing beside the views' one.
    for machinery in ["phase_durations", "arrayFilter", "mapContains"] {
        assert!(
            !QUERIES.contains(machinery),
            "a story-point query re-derives filter machinery ('{machinery}') from the raw \
             record; the views own the clean-landing verdict, the queries own the questions"
        );
    }
}

#[test]
fn the_views_deduplicate_at_least_once_redelivery() {
    // The extraction views collapse to one row per ship by grouping on the
    // ship identity; a distribution over un-deduplicated rows would count a
    // replayed landing twice (the raw table really does carry duplicates: the
    // derivation window held 10 579 rows / 10 578 distinct sweeps).
    for (label, sql) in [
        ("clickstack/story-point-extract.sql", CLICKSTACK_EXTRACT),
        ("signoz/story-point-extract.sql", SIGNOZ_EXTRACT),
    ] {
        let group_by = sql
            .lines()
            .find(|line| line.starts_with("GROUP BY"))
            .unwrap_or_else(|| panic!("{label} no longer collapses redeliveries with GROUP BY"));
        assert!(
            group_by.contains("repo") && group_by.contains("sweep_id"),
            "{label} groups its deduplication on something other than the ship identity"
        );
    }
}

#[test]
fn the_calibration_questions_bind_the_rubrics_cut_points() {
    for id in ["SP4", "SP5"] {
        for param in CUT_PARAMS {
            assert!(
                question_body(id).contains(&format!("{{{param}:UInt64}}")),
                "{id} no longer binds the token cut parameter {param}; the #9434 calibration \
                 loop re-runs these questions with candidate bounds instead of editing SQL"
            );
        }
    }
}

#[test]
fn the_rubric_cites_its_derivation_and_stays_provisional() {
    // The issue's acceptance criterion: bucket bounds are derived from
    // extracted distributions, not invented — the doc cites the query output
    // it came from, states the window and n, and names the calibration loop
    // that is allowed to revise it.
    assert!(
        RUBRIC.contains("story-point-queries.sql"),
        "story-points.md does not cite the question set its bounds came from"
    );
    assert!(
        RUBRIC.contains("SP2"),
        "story-points.md does not cite the distribution query (SP2) its bounds came from"
    );
    assert!(
        RUBRIC.contains("#9434"),
        "story-points.md does not name the #9434 calibration loop as its revision path"
    );
    let window = Regex::new(r"2026-09-14").unwrap();
    assert!(
        window.is_match(RUBRIC),
        "story-points.md does not state the derivation window's start"
    );
}
