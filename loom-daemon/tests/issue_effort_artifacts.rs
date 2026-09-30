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
//! exactly the shape that drifts silently: a trigger added to the vocabulary
//! and not to the query lands in the query's `ELSE 'unattributed'` arm, which
//! produces a *plausible smaller number* rather than an error. Nothing
//! reports that.
//!
//! So the authority here is the Rust source, and both the doc and the SQL are
//! checked against it, never against each other (following
//! [`cycle_time_artifacts`] #8665 and [`pr_latency_artifacts`] #8923 — and
//! `sweep_facts_artifacts.rs` from #9446). On `main`'s string-constants design
//! the authorities are:
//!
//! * the trigger vocabulary — the `pub const`s of `pub mod trigger` in
//!   `src/telemetry/mod.rs` (the strings the record actually carries);
//! * the rework kinds and their default substantive/environmental class —
//!   the match arms of `default_classification` in
//!   `src/sweep_registry/outcome_journal/rework.rs`;
//! * the payload field names — the serde fields of `SweepOutcomeRecord` and
//!   `ReworkEvent` in `src/telemetry/mod.rs` (a renamed field is what the
//!   `json_extract` paths in the query must be checked against).
//!
//! **This is the static half only.** A vocabulary that matches proves nothing
//! about whether the SQL runs or whether its arithmetic adds up;
//! `issue_effort_sqlite.rs` executes the same file verbatim on the bundled
//! SQLite against the real D1 `records` DDL and asserts the split. Read the
//! two together.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use regex::Regex;

const TELEMETRY: &str = include_str!("../src/telemetry/mod.rs");
const REWORK: &str = include_str!("../src/sweep_registry/outcome_journal/rework.rs");
const SCHEMA: &str = include_str!("../../defaults/docs/telemetry-schema.md");
const QUERIES: &str = include_str!("../../defaults/observability/issue-effort-queries.sql");
const LABELS: &str = include_str!("../../.github/labels.yml");

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

/// The payload fields the query reads off a `sweep.outcome` record, and the
/// fields it reads off a `rework_events[]` entry. Checked against the serde
/// structs, not just against the query's own text.
const RECORD_FIELDS: &[&str] = &[
    "trigger",
    "attempt_index",
    "previous_sweep_id",
    "rework_events",
    "total_duration_sec",
    "disposition",
    "pr_number",
];

const EVENT_FIELDS: &[&str] = &["kind", "classification", "duration_sec"];

/// Every wire string in the `pub mod trigger` const block of `source` — the
/// trigger vocabulary the record actually carries.
fn trigger_strings(source: &str) -> BTreeSet<String> {
    let start = source
        .find("pub mod trigger {")
        .expect("`pub mod trigger` block must exist in src/telemetry/mod.rs");
    let end = start
        + source[start..]
            .find("\n}")
            .expect("`pub mod trigger` must terminate");
    Regex::new(r#"pub const \w+: &str = "([a-z0-9_]+)";"#)
        .unwrap()
        .captures_iter(&source[start..end])
        .map(|capture| capture[1].to_owned())
        .collect()
}

/// Every `(kind, default_class)` pair in `default_classification`'s match
/// arms, parsed rather than restated: an arm added to the table is picked up
/// here, and a kind the table does not classify cannot pass the schema-doc
/// check below.
fn default_classifications(source: &str) -> Vec<(String, String)> {
    let start = source
        .find("pub(crate) fn default_classification")
        .expect("`default_classification` must exist in outcome_journal/rework.rs");
    let end = start
        + source[start..]
            .find("\n}")
            .expect("`default_classification` must terminate");
    let string_re = Regex::new(r#""([a-z_]+)""#).unwrap();
    let mut pairs = Vec::new();
    for line in source[start..end].lines() {
        let Some((left, right)) = line.split_once("=>") else {
            continue;
        };
        let class = right.trim().trim_end_matches(',').trim().trim_matches('"');
        if class != "environmental" && class != "substantive" {
            continue;
        }
        for capture in string_re.captures_iter(left) {
            let kind = &capture[1];
            if kind != "_" {
                pairs.push((kind.to_owned(), class.to_owned()));
            }
        }
    }
    assert!(
        pairs.len() >= 4,
        "expected the full rework-kind classification table, found {pairs:?}"
    );
    pairs
}

/// The serde field names of `pub struct <Name>` in `source`.
fn struct_fields(source: &str, struct_name: &str) -> BTreeSet<String> {
    let marker = format!("pub struct {struct_name} {{");
    let start = source
        .find(&marker)
        .unwrap_or_else(|| panic!("`{marker}` must exist in src/telemetry/mod.rs"));
    let end = start
        + source[start..]
            .find("\n}")
            .expect("the struct must terminate");
    Regex::new(r#"pub (\w+):"#)
        .unwrap()
        .captures_iter(&source[start..end])
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

/// Every trigger the daemon can write is classified by the query — or is one
/// of the deliberately-unbucketed ones. A trigger that is neither would
/// silently fall into `ELSE 'unattributed'` and shrink every rework total.
#[test]
fn every_trigger_is_bucketed_by_the_query_or_deliberately_not() {
    let triggers = trigger_strings(TELEMETRY);
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

/// …and the schema doc names every one of them, so the vocabulary an operator
/// reads is the vocabulary the wire carries.
#[test]
fn the_schema_doc_documents_every_trigger_and_rework_kind() {
    for value in trigger_strings(TELEMETRY) {
        assert!(
            SCHEMA.contains(&value),
            "trigger `{value}` is missing from the trigger vocabulary in \
             defaults/docs/telemetry-schema.md"
        );
    }
    for (kind, _) in default_classifications(REWORK) {
        assert!(
            SCHEMA.contains(&kind),
            "rework kind `{kind}` is missing from the rework-event documentation in \
             defaults/docs/telemetry-schema.md"
        );
    }
}

/// The two classes are the query's only bucket names, and both are the classes
/// the daemon's own classification table defaults to. A renamed class would
/// otherwise match nothing in the JSON and report every issue as clean.
#[test]
fn the_query_reads_the_classes_the_daemon_defaults_to() {
    let classes: BTreeSet<String> = default_classifications(REWORK)
        .into_iter()
        .map(|(_, class)| class)
        .collect();
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

/// The query reads the record's field names, not approximations of them — and
/// those field names are the serde fields the record actually serializes. A
/// renamed field yields zero rows forever — indistinguishable from "the fleet
/// had no rework", which is the failure mode this whole issue exists to end.
#[test]
fn the_query_reads_the_fields_the_record_carries() {
    let record = struct_fields(TELEMETRY, "SweepOutcomeRecord");
    let event = struct_fields(TELEMETRY, "ReworkEvent");
    for field in RECORD_FIELDS {
        assert!(
            record.contains(*field),
            "SweepOutcomeRecord no longer carries `{field}` — update RECORD_FIELDS \
             or the query, deliberately"
        );
        assert!(
            QUERIES.contains(&format!("'$.{field}'")),
            "issue-effort-queries.sql never reads `{field}` off the payload"
        );
    }
    for field in EVENT_FIELDS {
        assert!(
            event.contains(*field),
            "ReworkEvent no longer carries `{field}` — update EVENT_FIELDS or the \
             query, deliberately"
        );
        assert!(
            QUERIES.contains(&format!("'$.{field}'")),
            "issue-effort-queries.sql never reads `{field}` off a rework event"
        );
    }
}

/// The labels the rework kinds and triggers are named after are the ones this
/// repo actually applies. A label renamed in `.github/labels.yml` without
/// updating the performing paths would silently stop producing rework events
/// whose `reason` an operator can trace back to its forge cause.
#[test]
fn the_rework_labels_exist_in_the_label_catalog() {
    for label in [
        "loom:merge-conflict",
        "loom:ci-failure",
        "loom:changes-requested",
        "loom:review-requested",
        "loom:pr",
    ] {
        assert!(
            LABELS.contains(&format!("- name: {label}\n")),
            "the rework vocabulary keys on `{label}`, which .github/labels.yml \
             does not define"
        );
    }
}
