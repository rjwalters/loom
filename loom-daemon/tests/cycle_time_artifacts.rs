//! Static contract for the cycle-time analytics artifacts (Issue #8665). No
//! Docker, network, backend or credential is used, so this runs in ordinary CI
//! on any host; the live half is `cycle_time_clickhouse.rs`.
//!
//! Why this exists. The design's whole claim is that ClickStack and SigNoz
//! answer the *same* canonical questions because only one small view differs
//! between them (`loom_analytics.raw_ship_outcome`) and everything downstream —
//! the durable table, the ingest, and all eight CT queries — is shared. That
//! claim is one careless edit away from being false in a way nothing reports: a
//! column renamed on one side only, an attribute key the gateway strips (which
//! yields zero rows forever, indistinguishable from "the fleet shipped
//! nothing"), or a question documented in prose that no query implements.
//!
//! As in [`signoz_trial_artifacts`], the authorities are derived rather than
//! restated: the column list comes from the rollup DDL, the forwarding
//! allowlist from the gateway config the deployment actually mounts, and the
//! raw retention figure from the Compose file that sets it.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;

use regex::Regex;

const QUESTIONS: &str = include_str!("../../defaults/observability/cycle-time-questions.md");
const ROLLUP: &str = include_str!("../../defaults/observability/cycle-time-rollup.sql");
const QUERIES: &str = include_str!("../../defaults/observability/cycle-time-queries.sql");
const CLICKSTACK_EXTRACT: &str =
    include_str!("../../defaults/observability/clickstack/cycle-time-extract.sql");
const SIGNOZ_EXTRACT: &str =
    include_str!("../../defaults/observability/signoz/cycle-time-extract.sql");
const COLLECTOR_CONFIG: &str = include_str!("../../defaults/observability/collector/config.yaml");
const CLICKSTACK_COMPOSE: &str =
    include_str!("../../defaults/observability/clickstack/compose.yaml");

/// The canonical question IDs. Restated here deliberately: this is the one
/// place the *set* is pinned, and every artifact below is checked against it
/// rather than against another artifact, so two files cannot drift together.
const QUESTION_IDS: &[&str] = &["CT1", "CT2", "CT3", "CT4", "CT5", "CT6", "CT7", "CT8"];

/// Output aliases of an extraction view: every `… AS name` at the end of a line
/// between the top-level `SELECT` and its `FROM`. Order is preserved because
/// the rollup's `INSERT` lists the same names and a silent transposition is
/// exactly what an unordered comparison would miss.
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

/// The rollup table's own column names, from its `CREATE TABLE` body.
fn table_columns() -> Vec<String> {
    let start = ROLLUP
        .find("CREATE TABLE IF NOT EXISTS loom_analytics.ship_cycle_time")
        .expect("rollup DDL no longer creates ship_cycle_time");
    let body = &ROLLUP[start..];
    let open = body.find("(\n").unwrap();
    let close = body.find("\n)").unwrap();
    Regex::new(r"(?m)^\s{4}([a-z_][a-z0-9_]*)\s")
        .unwrap()
        .captures_iter(&body[open..close])
        .map(|capture| capture[1].to_owned())
        .collect()
}

/// The column list of the rollup's explicit `INSERT INTO … (…)`.
fn insert_columns() -> Vec<String> {
    let start = ROLLUP
        .find("INSERT INTO loom_analytics.ship_cycle_time")
        .expect("rollup no longer has an explicit INSERT");
    let body = &ROLLUP[start..];
    let list = &body[body.find('(').unwrap() + 1..body.find(')').unwrap()];
    list.split(',').map(|name| name.trim().to_owned()).collect()
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

#[test]
fn both_backends_extract_the_same_normalized_ship_columns() {
    let clickstack = view_output_columns(CLICKSTACK_EXTRACT);
    let signoz = view_output_columns(SIGNOZ_EXTRACT);
    assert!(
        !clickstack.is_empty(),
        "parsed no output columns from the ClickStack extraction view"
    );
    assert_eq!(
        clickstack, signoz,
        "the ClickStack and SigNoz extraction views no longer expose the same ship columns in \
         the same order, so the shared rollup cannot ingest both and cross-backend parity is \
         broken (#8529)"
    );
}

#[test]
fn the_rollup_ingests_exactly_the_columns_both_views_expose() {
    let extracted = view_output_columns(CLICKSTACK_EXTRACT);
    assert_eq!(
        insert_columns(),
        extracted,
        "the rollup's INSERT column list and the extraction views disagree; values would be \
         written into neighbouring columns"
    );
    let mut declared = table_columns();
    declared.sort();
    let mut extracted = extracted;
    extracted.sort();
    assert_eq!(
        declared, extracted,
        "ship_cycle_time's own columns and the extracted ship columns disagree"
    );
    // The refreshable view re-declares the same list a third time (ClickHouse
    // requires the target column types on an APPEND view), so it is checked too.
    for column in table_columns() {
        let refresh = ROLLUP
            .split("CREATE MATERIALIZED VIEW")
            .nth(1)
            .expect("rollup no longer defines the refreshable view");
        assert!(refresh.contains(&column), "the refreshable view omits column '{column}'");
    }
}

#[test]
fn every_attribute_key_the_extractions_read_survives_the_gateway() {
    let log_keys = allowlist("log");
    let resource_keys = allowlist("resource");
    for (label, sql, signal, resource) in [
        (
            "clickstack/cycle-time-extract.sql",
            CLICKSTACK_EXTRACT,
            vec!["LogAttributes"],
            vec!["ResourceAttributes"],
        ),
        (
            "signoz/cycle-time-extract.sql",
            SIGNOZ_EXTRACT,
            vec!["attributes_string", "attributes_number"],
            vec!["resources_string"],
        ),
    ] {
        let read = referenced_keys(sql, &signal);
        assert!(
            read.iter().any(|key| key == "loom.phase_durations"),
            "{label} no longer reads loom.phase_durations, the core cycle-time signal"
        );
        for key in read {
            assert!(
                log_keys.contains(&key),
                "{label} reads attribute '{key}', which the gateway's keep_keys allowlist strips \
                 — the query can only ever return zero rows, which is indistinguishable from a \
                 fleet that shipped nothing"
            );
        }
        for key in referenced_keys(sql, &resource) {
            assert!(
                resource_keys.contains(&key),
                "{label} reads resource key '{key}', which the gateway does not forward"
            );
        }
    }
}

#[test]
fn the_documented_question_set_is_the_implemented_one() {
    for id in QUESTION_IDS {
        assert!(
            QUESTIONS.contains(&format!("**{id}**")),
            "cycle-time-questions.md documents no {id}"
        );
        let implementations = QUERIES.matches(&format!("-- {id}. ")).count();
        assert_eq!(
            implementations, 1,
            "cycle-time-queries.sql implements {id} {implementations} times; expected exactly once"
        );
    }
    let stray = Regex::new(r"(?m)^-- (CT\d+)\. ").unwrap();
    for capture in stray.captures_iter(QUERIES) {
        assert!(
            QUESTION_IDS.contains(&&capture[1]),
            "cycle-time-queries.sql implements {}, which is not in the canonical question set",
            &capture[1]
        );
    }
}

#[test]
fn every_question_binds_its_window_instead_of_hardcoding_one() {
    // Cut on the `-- CTn. ` markers rather than counting occurrences globally:
    // CT8 legitimately binds `{since:DateTime}` twice (once per side of its
    // `FULL OUTER JOIN`), which is still a bound parameter reused twice, not a
    // hand-edited literal — a global count-equals-question-count check would
    // reject that as if it were under-parameterized.
    let mut markers: Vec<usize> = QUESTION_IDS
        .iter()
        .map(|id| QUERIES.find(&format!("-- {id}. ")).unwrap())
        .collect();
    markers.push(QUERIES.len());
    for window in QUESTION_IDS.iter().zip(markers.windows(2)) {
        let (id, bounds) = window;
        let body = &QUERIES[bounds[0]..bounds[1]];
        assert!(
            body.contains("{since:DateTime}"),
            "{id} does not bind its window as a parameter; editing a date literal into the SQL \
             is the hand-written-SQL habit these artifacts replace"
        );
    }
    let date_literal = Regex::new(r"(?m)^[^-].*'20\d\d-\d\d-\d\d").unwrap();
    assert!(
        date_literal.find(QUERIES).is_none(),
        "a date literal was written into a cycle-time query outside a comment"
    );
}

#[test]
fn the_retention_decision_matches_the_ttls_it_reasons_about() {
    let raw_ttl =
        Regex::new(r"HYPERDX_OTEL_EXPORTER_TABLES_TTL:\s*\$\{CLICKSTACK_RETENTION:-(\w+)\}")
            .unwrap()
            .captures(CLICKSTACK_COMPOSE)
            .expect("the ClickStack compose file no longer sets the exporter table TTL")
            .get(1)
            .unwrap()
            .as_str()
            .to_owned();
    assert!(
        QUESTIONS.contains(&format!("`{raw_ttl}`")),
        "cycle-time-questions.md argues about a raw retention of something other than the {raw_ttl} \
         the deployment actually configures"
    );
    let rollup_ttl = Regex::new(r"TTL toDateTime\(finished_at\) \+ INTERVAL (\d+) DAY")
        .unwrap()
        .captures(ROLLUP)
        .expect("the rollup table no longer declares its own TTL")
        .get(1)
        .unwrap()
        .as_str()
        .to_owned();
    assert!(
        QUESTIONS.contains(&format!("{rollup_ttl} days")),
        "cycle-time-questions.md claims a rollup retention other than the {rollup_ttl} days the \
         DDL sets"
    );
}

#[test]
fn reads_go_through_the_deduplicating_view_not_the_base_table() {
    // At-least-once delivery means the base table holds duplicate rows; only
    // the `ship` view applies FINAL. A question that reads the base table
    // directly would double-count a replayed sweep.
    assert!(
        !QUERIES.contains("FROM loom_analytics.ship_cycle_time"),
        "a cycle-time query reads the base table directly; duplicate deliveries would be \
         counted twice — read loom_analytics.ship (FINAL) instead"
    );
    assert!(
        ROLLUP.contains("FROM loom_analytics.ship_cycle_time FINAL"),
        "the ship view no longer deduplicates with FINAL"
    );
}
