//! Static contract for the ETA artifacts (Issue #9330, consolidated into
//! #9329). No Docker, network, forge or credential is used, so this runs in
//! ordinary CI on any host.
//!
//! Why this exists. `eta-queries.sql` is executed against SigNoz by hand,
//! months after it was written, by someone who did not write it. Nothing in
//! ClickHouse validates an attribute name: a query that reads
//! `attributes_number['loom.eta.error_sec']` after that key is renamed
//! returns a column of zeroes and empty groups — a *plausible* answer, not an
//! error. The same is true of the documentation: an `eta.md` that describes a
//! field the daemon no longer emits reads exactly like one that is correct.
//!
//! So the three artifacts are tied to the emitted schema here: every
//! attribute the queries read must be one the ETA mapping actually emits and
//! the collector is allowed to keep, and every field of the `eta.snapshot`
//! row must be documented where the dashboard's implementer will look for it.
//!
//! Follows the shape of [`pr_latency_artifacts`] (#8923) and
//! [`cycle_time_artifacts`] (#8665): the committed artifacts are the
//! authorities, each checked against a *different* one (or against the live
//! Rust types), so two files cannot drift together into agreeing on the wrong
//! thing.
#![allow(clippy::unwrap_used)]

use loom_daemon::telemetry::kinds::eta::ETA_LOG_ATTRIBUTE_KEYS;
use loom_daemon::telemetry::kinds::eta_snapshot::{EtaSnapshotRecord, EtaSnapshotRow};

const QUERIES: &str = include_str!("../../defaults/observability/signoz/eta-queries.sql");
const ETA_DOC: &str = include_str!("../../defaults/docs/eta.md");
const SCHEMA_DOC: &str = include_str!("../../defaults/docs/telemetry-schema.md");
/// The one place `loom.eta.*` attribute *values* are produced.
const MAPPING: &str = include_str!("../src/observability/otlp/mapping/eta.rs");
/// The `land-v1` golden explanation — the body Q3 expands `features` out of.
const EXPLANATION_GOLDEN: &str = include_str!("../src/eta/fixtures/explanation-golden.json");

/// The generic per-record attributes the queries may read besides the
/// `loom.eta.*` family.
const GENERIC_KEYS: &[&str] = &["loom.repo", "loom.issue", "loom.pr_number"];

/// Every `attributes_{string,number,bool}['key']` and
/// `mapContains(attributes_*, 'key')` the SQL reads, in order, deduplicated.
fn attribute_keys(sql: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for (index, _) in sql.match_indices("attributes_") {
        let rest = &sql[index..];
        // `attributes_string['loom.eta.kind']` or
        // `mapContains(attributes_string, 'loom.eta.kind')`.
        let Some(quote) = rest.find('\'') else {
            continue;
        };
        // Only look ahead a little: a quote much further on belongs to some
        // other expression, not to this map reference.
        if quote > 40 {
            continue;
        }
        let after = &rest[quote + 1..];
        let Some(end) = after.find('\'') else {
            continue;
        };
        let key = after[..end].to_string();
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// The four attribute names [`MAPPING`]'s `provenance` helper builds from a
/// prefix rather than from a whole literal.
const PROVENANCE_SUFFIXES: &[&str] = &["version", "revision", "tree_state", "provenance_complete"];

/// Every `loom.eta.*` attribute the OTLP mapping emits: the whole-key string
/// literals, plus the prefix-built provenance keys (`loom.eta.version`,
/// `loom.eta.outcome_revision`, …), which never appear as one literal.
fn emitted_keys(src: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for (index, _) in src.match_indices("\"loom.eta.") {
        let after = &src[index + 1..];
        if let Some(end) = after.find('"') {
            keys.push(after[..end].to_string());
        }
    }
    // The helper's own body is the authority for the suffix list, so the
    // expansion below cannot silently go stale.
    for suffix in PROVENANCE_SUFFIXES {
        assert!(
            src.contains(&format!("{{prefix}}{suffix}")),
            "mapping/eta.rs's `provenance` helper no longer builds `{suffix}` from the \
             prefix; update PROVENANCE_SUFFIXES"
        );
    }
    for (index, _) in src.match_indices("provenance(&mut attributes, \"") {
        let after = &src[index..];
        let start = after.find('"').unwrap() + 1;
        let end = after[start..].find('"').unwrap();
        let prefix = &after[start..start + end];
        for suffix in PROVENANCE_SUFFIXES {
            keys.push(format!("{prefix}{suffix}"));
        }
    }
    keys
}

/// The `-- 0.` / `-- Qn.` section headers the query file defines.
fn sections(sql: &str) -> Vec<String> {
    sql.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("-- ")?;
            let head = rest.split_whitespace().next()?;
            let stripped = head.strip_suffix('.')?;
            (stripped == "0"
                || stripped
                    .strip_prefix('Q')
                    .is_some_and(|n| n.parse::<u8>().is_ok()))
            .then(|| stripped.to_string())
        })
        .collect()
}

#[test]
fn the_queries_find_attribute_references_at_all() {
    // The scanner below is the basis of two other tests; a silent zero-match
    // would make both of them vacuous.
    let keys = attribute_keys(QUERIES);
    assert!(keys.len() > 10, "only found {keys:?} — the attribute scanner is broken");
}

/// The drift this file exists to catch: a query reading an attribute nothing
/// emits (or that the collector drops) returns zeroes and empty groups, never
/// an error.
#[test]
fn every_attribute_the_queries_read_is_one_the_daemon_emits() {
    for key in attribute_keys(QUERIES) {
        if GENERIC_KEYS.contains(&key.as_str()) {
            continue;
        }
        assert!(
            key.starts_with("loom.eta."),
            "eta-queries.sql reads `{key}`, which is neither a generic record attribute \
             ({GENERIC_KEYS:?}) nor part of the `loom.eta.*` family"
        );
        assert!(
            ETA_LOG_ATTRIBUTE_KEYS.contains(&key.as_str()),
            "eta-queries.sql reads `{key}`, which is not in ETA_LOG_ATTRIBUTE_KEYS — the \
             collector's transform/privacy `keep_keys` therefore drops it, and the query \
             silently returns nothing for that column"
        );
        assert!(
            emitted_keys(MAPPING).contains(&key),
            "eta-queries.sql reads `{key}`, which observability/otlp/mapping/eta.rs never \
             emits — the column would always be empty"
        );
    }
}

/// The reverse direction is deliberately NOT asserted (an emitted attribute
/// no query reads is fine — the dashboards and ad-hoc queries read them too),
/// but a `loom.eta.*` key the collector allowlist carries and the ETA mapping
/// never emits is always a mistake in one of the two. Keys outside that
/// family (`loom.story`) are emitted by the shared metadata layer
/// (`observability::lifecycle`), not here, so they are not this test's.
#[test]
fn every_allowlisted_eta_attribute_is_actually_emitted() {
    let emitted = emitted_keys(MAPPING);
    let family: Vec<&&str> = ETA_LOG_ATTRIBUTE_KEYS
        .iter()
        .filter(|key| key.starts_with("loom.eta."))
        .collect();
    assert!(family.len() > 20, "the allowlist lost most of the family: {family:?}");
    for key in family {
        assert!(
            emitted.iter().any(|e| e == key),
            "ETA_LOG_ATTRIBUTE_KEYS carries `{key}`, which the OTLP mapping never emits: \
             either the mapping lost it or the allowlist kept a stale name"
        );
    }
}

#[test]
fn the_query_file_and_the_doc_describe_the_same_question_set() {
    let sections = sections(QUERIES);
    assert_eq!(
        sections,
        vec!["0", "Q1", "Q2", "Q3"],
        "eta-queries.sql's sections changed; update the Queries section of eta.md with them"
    );
    for id in ["**Section 0**", "**Q1**", "**Q2**", "**Q3**"] {
        assert!(ETA_DOC.contains(id), "eta.md's Queries section does not describe {id}");
    }
    assert!(
        ETA_DOC.contains("eta-queries.sql"),
        "eta.md must link the query file it describes"
    );
}

/// Q3 expands the estimate record's body — which is the explanation — so the
/// `features` object it names has to be where it looks, and its values have
/// to be the shape the query's `toFloat64OrNull` choice was made for.
#[test]
fn the_feature_ranking_query_matches_the_explanation_fixture() {
    assert!(
        QUERIES.contains("JSONExtractKeysAndValuesRaw(e.body, 'features')"),
        "Q3 must expand `features` out of the estimate's body"
    );
    assert!(
        QUERIES.contains("toFloat64OrNull(kv.2)") && QUERIES.contains("WHERE value IS NOT NULL"),
        "the typed extraction would read an unmeasured (null) feature as 0; Q3 must keep \
         the raw form plus toFloat64OrNull and drop the nulls"
    );
    let golden: serde_json::Value = serde_json::from_str(EXPLANATION_GOLDEN).unwrap();
    let features = golden["features"]
        .as_object()
        .expect("the golden explanation carries a top-level `features` object");
    assert!(
        features.values().any(serde_json::Value::is_number),
        "no numeric feature in the fixture — Q3's correlation would have nothing to rank"
    );
    assert!(
        features.values().any(serde_json::Value::is_null),
        "no unmeasured feature in the fixture — the null-vs-zero rule Q3 is written around \
         is then never exercised"
    );
    // And the join key the query uses to pair an outcome with its estimate.
    assert!(golden["estimate_id"].is_string());
    assert!(QUERIES.contains("ON o.estimate_id = e.estimate_id"));
}

/// `abandoned` outcomes and refusals are counted and never scored. Both
/// halves have to hold at once: the mapping must omit the error fields
/// (absent, not zero) and the queries must filter on their presence.
#[test]
fn the_scored_versus_counted_split_is_present_on_both_sides() {
    assert!(
        MAPPING.contains("opt_int(&mut attributes, \"loom.eta.error_sec\""),
        "error_sec must be emitted only when present — a zero would be scored as perfect"
    );
    assert!(
        QUERIES.contains("mapContains(attributes_number, 'loom.eta.error_sec')"),
        "the scored sections must filter on the error field's presence"
    );
    assert!(
        QUERIES.contains("'abandoned'"),
        "section 0 must count abandonments, which are never scored"
    );
    assert!(
        ETA_DOC.contains("absent is never zero") || ETA_DOC.contains("absent is never zero:"),
        "eta.md must keep the absent-vs-zero rule the queries depend on"
    );
}

/// Every field of the live list must be documented where the dashboard's
/// implementer looks for it. Checked against the **serialized row**, not
/// against a list written out here, so adding a field to the record without
/// documenting it fails.
#[test]
fn every_eta_snapshot_field_is_documented_in_the_schema_reference() {
    let row = EtaSnapshotRow {
        repo: "rjwalters/loom".to_string(),
        visibility: loom_daemon::telemetry::RepoVisibility::Public,
        issue: 9329,
        pr: Some(9755),
        kind: loom_daemon::eta::Kind::Land,
        p25: Some(1_200),
        p50: Some(3_600),
        p75: Some(9_000),
        heuristic: "land-v1".to_string(),
        estimate_id: "b3c1d2e4f5a60718".to_string(),
        as_of: chrono::Utc::now(),
        stage: Some(loom_daemon::eta::Stage::ReviewWait),
        no_estimate_reason: None,
    };
    let record = EtaSnapshotRecord {
        as_of: row.as_of,
        rows: vec![row.clone()],
        rows_truncated: 0,
    };
    let section = SCHEMA_DOC
        .split("### `eta.snapshot`")
        .nth(1)
        .expect("telemetry-schema.md documents the eta.snapshot kind")
        .split("\n### ")
        .next()
        .unwrap();

    // A list field is documented as `rows[]`, a scalar as `rows` — accept
    // either spelling, but require one of them.
    let documented = |field: &str| {
        section.contains(&format!("`{field}`")) || section.contains(&format!("`{field}[]`"))
    };
    for field in serde_json::to_value(&record)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
    {
        assert!(
            documented(field),
            "eta.snapshot's `{field}` is not documented in telemetry-schema.md"
        );
    }
    for field in serde_json::to_value(&row)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
    {
        assert!(
            documented(field),
            "the eta.snapshot row's `{field}` is not documented in telemetry-schema.md"
        );
    }
    // The routing decision, which is the one thing a dashboard cannot infer
    // from the field list.
    assert!(
        section.contains("Native-HTTPS only"),
        "the section must say the kind never reaches OTLP"
    );
    assert!(
        section.contains("estimate_id"),
        "the section must name the on-demand explanation lookup's join key"
    );
}

/// The `eta.md` forward references that #9329 / #9330 retire. A doc that
/// still calls a shipped surface "a later phase" is worse than one that omits
/// it: it tells the reader not to look.
#[test]
fn the_doc_no_longer_defers_the_live_list_to_a_later_phase() {
    assert!(
        !ETA_DOC.contains("Later phases"),
        "eta.md still frames shipped work as later phases"
    );
    assert!(
        ETA_DOC.contains("## The live list (`eta.snapshot`)"),
        "eta.md must document the live list the dashboard reads"
    );
    assert!(
        ETA_DOC.contains("### Adding a v2, and comparing it"),
        "eta.md must document how a candidate heuristic is compared and promoted"
    );
    // Both halves of the promotion gate, since either alone is not the rule.
    assert!(ETA_DOC.contains("50 paired observations") && ETA_DOC.contains("[40%, 60%]"));
}
