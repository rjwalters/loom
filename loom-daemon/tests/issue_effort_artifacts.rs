//! Static contract for the per-issue effort artifacts (Issue #9444). No
//! Docker, network, forge, backend or credential is used, so this runs in
//! ordinary CI on any host.
//!
//! Why this exists. The whole claim of `attempt_index` / `trigger` /
//! `rework_events` is that **one** classification table decides what counts as
//! substantive rework and what counts as environmental — and that the same
//! table is applied by the daemon that writes the records, by the schema doc
//! an operator reads a number out of, and by the committed query that turns
//! the records into a per-issue split. Three restatements of one table is
//! exactly the shape that drifts silently: a variant added to the enum and not
//! to the query lands in the query's `ELSE 'unattributed'` arm, which produces
//! a *plausible smaller number* rather than an error. Nothing reports that.
//!
//! So the authority here is the Rust source — `lineage.rs`'s `as_str()` match
//! arms, which are the wire strings by construction — and both the doc and the
//! SQL are checked against it. Following [`cycle_time_artifacts`] (#8665) and
//! [`pr_latency_artifacts`] (#8923): the artifacts are compared against the
//! implementation, never against each other, so two files cannot drift
//! together into agreeing on the wrong thing.
//!
//! **This is the static half only.** A vocabulary that matches proves nothing
//! about whether the SQL runs or whether its arithmetic adds up;
//! `issue_effort_sqlite.rs` executes the same file verbatim on the bundled
//! SQLite against the real D1 `records` DDL and asserts the split. Read the two
//! together — the first draft of this query set passed every check in this file
//! while its documented runner bound no parameters at all.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;

use regex::Regex;

const LINEAGE: &str = include_str!("../src/telemetry/lineage.rs");
const REWORK: &str = include_str!("../src/sweep_registry/outcome_journal/rework.rs");
const SCHEMA: &str = include_str!("../../defaults/docs/telemetry-schema.md");
const QUERIES: &str = include_str!("../../defaults/observability/issue-effort-queries.sql");

/// The canonical question IDs. Pinned here deliberately: this is the one place
/// the *set* is fixed, and every artifact below is checked against it rather
/// than against another artifact.
const QUESTION_IDS: &[&str] = &["IE1", "IE2", "IE3", "IE4", "IE5"];

/// The triggers that are, by design, NOT charged to either rework bucket — so
/// the query's `ELSE 'unattributed'` arm is their correct home and their
/// absence from its `CASE` list is not drift. Pinned here so that *adding* a
/// variant to this list is a visible, reviewable diff rather than a silent
/// widening of the unattributed bucket.
const UNBUCKETED_TRIGGERS: &[&str] = &["operator_redispatch", "unknown"];

/// Every wire string produced by an `as_str()` match arm in `source` whose
/// enum is named `enum_name` — i.e. the vocabulary the wire actually carries.
///
/// Derived rather than restated: an enum variant with no `as_str()` arm does
/// not compile (the match is exhaustive), so this cannot miss one.
fn wire_strings(source: &str, enum_name: &str) -> BTreeSet<String> {
    let impl_start = source
        .find(&format!("impl {enum_name} {{"))
        .unwrap_or_else(|| panic!("no `impl {enum_name}` block in the source"));
    let as_str = source[impl_start..]
        .find("pub fn as_str(self)")
        .unwrap_or_else(|| panic!("{enum_name} has no as_str()"))
        + impl_start;
    // The arms run to the end of the match block; the next `    }` at impl
    // indentation closes the function.
    let end = source[as_str..]
        .find("\n    }\n")
        .unwrap_or_else(|| panic!("{enum_name}::as_str() is not shaped as expected"))
        + as_str;
    Regex::new(r#"Self::\w+ => "([a-z0-9_]+)""#)
        .unwrap()
        .captures_iter(&source[as_str..end])
        .map(|capture| capture[1].to_owned())
        .collect()
}

#[test]
fn every_question_is_implemented_by_the_query_file() {
    for id in QUESTION_IDS {
        assert!(
            QUERIES.contains(&format!("-- {id}.")),
            "{id} is part of the canonical set but no statement in \
             issue-effort-queries.sql implements it"
        );
    }
}

#[test]
fn the_schema_doc_names_the_query_file_and_its_questions() {
    assert!(
        SCHEMA.contains("issue-effort-queries.sql"),
        "the schema doc must point at the committed query file, or an operator \
         reading a number has no definition to read it against"
    );
    for id in QUESTION_IDS {
        assert!(
            SCHEMA.contains(id),
            "{id} exists in the query file but the schema doc does not mention it"
        );
    }
}

/// Every `SweepTrigger` the daemon can write is classified by the query — or
/// is one of the deliberately-unbucketed ones. A variant that is neither would
/// silently fall into `ELSE 'unattributed'` and shrink every rework total.
#[test]
fn every_trigger_is_bucketed_by_the_query_or_deliberately_not() {
    let triggers = wire_strings(LINEAGE, "SweepTrigger");
    assert!(triggers.len() >= 10, "expected the full trigger vocabulary, found {triggers:?}");
    for trigger in &triggers {
        if UNBUCKETED_TRIGGERS.contains(&trigger.as_str()) {
            assert!(
                !QUERIES.contains(&format!("WHEN '{trigger}'")),
                "{trigger} is pinned as unbucketed but the query gives it a bucket; \
                 update UNBUCKETED_TRIGGERS or the query, deliberately"
            );
            continue;
        }
        assert!(
            QUERIES.contains(&format!("WHEN '{trigger}'")),
            "trigger `{trigger}` is emitted by the daemon but issue-effort-queries.sql \
             never classifies it — it would land in ELSE 'unattributed' and quietly \
             shrink the rework totals"
        );
    }
}

/// …and the schema doc's normative table names every one of them, so the
/// vocabulary an operator reads is the vocabulary the wire carries.
#[test]
fn the_schema_doc_documents_every_trigger_and_rework_kind() {
    for value in wire_strings(LINEAGE, "SweepTrigger") {
        assert!(
            SCHEMA.contains(&format!("| `{value}` |")),
            "trigger `{value}` is missing from the classification table in \
             defaults/docs/telemetry-schema.md"
        );
    }
    for value in wire_strings(LINEAGE, "ReworkKind") {
        assert!(
            SCHEMA.contains(&format!("| `{value}` |")),
            "rework kind `{value}` is missing from the classification table in \
             defaults/docs/telemetry-schema.md"
        );
    }
}

/// The two classes are the query's only bucket names. A renamed class would
/// otherwise match nothing in the JSON and report every issue as clean.
#[test]
fn the_query_reads_the_classes_the_daemon_writes() {
    let classes = wire_strings(LINEAGE, "ReworkClass");
    assert_eq!(
        classes,
        ["environmental".to_owned(), "substantive".to_owned()]
            .into_iter()
            .collect::<BTreeSet<_>>()
    );
    for class in &classes {
        assert!(
            QUERIES.contains(&format!("'$.classification') = '{class}'")),
            "issue-effort-queries.sql never selects rework of class `{class}`"
        );
    }
}

/// The query reads the record's field names, not approximations of them. A
/// renamed field yields zero rows forever — indistinguishable from "the fleet
/// had no rework", which is the failure mode this whole issue exists to end.
#[test]
fn the_query_reads_the_fields_the_record_carries() {
    for field in [
        "trigger",
        "attempt_index",
        "previous_sweep_id",
        "rework_events",
        "total_duration_sec",
        "disposition",
    ] {
        assert!(
            QUERIES.contains(&format!("'$.{field}'")),
            "issue-effort-queries.sql never reads `{field}` off the payload"
        );
    }
    for field in ["kind", "classification", "duration_sec"] {
        assert!(
            QUERIES.contains(&format!("'$.{field}'")),
            "issue-effort-queries.sql never reads `{field}` off a rework event"
        );
    }
}

/// The labels the rework derivation keys on are the ones this repo actually
/// applies. A label renamed in `.github/labels.yml` without updating the
/// derivation would silently stop producing environmental rework.
#[test]
fn the_rework_labels_exist_in_the_label_catalog() {
    const LABELS: &str = include_str!("../../.github/labels.yml");
    for label in [
        "loom:merge-conflict",
        "loom:ci-failure",
        "loom:changes-requested",
        "loom:review-requested",
        "loom:pr",
    ] {
        assert!(
            LABELS.contains(&format!("- name: {label}\n")),
            "the rework derivation keys on `{label}`, which .github/labels.yml \
             does not define"
        );
        assert!(
            REWORK.contains(label),
            "`{label}` is expected by this contract but the rework derivation \
             does not mention it"
        );
    }
}
