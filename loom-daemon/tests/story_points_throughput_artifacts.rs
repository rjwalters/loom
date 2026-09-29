//! Static contract for the story-points throughput artifacts (Issue #9433,
//! epic #9429; pattern: #8665). No Docker, network, backend or credential is
//! used, so this runs in ordinary CI on any host. As in
//! [`cycle_time_artifacts`], the authorities are derived rather than
//! restated: the column list comes from the rollup DDL, the forwarding
//! allowlist from the gateway config the deployment actually mounts, and the
//! raw retention figure from the Compose file that sets it.
//!
//! Why this exists. The set's whole claim is that "story points landed per
//! day" means the same thing on ClickStack and on SigNoz because only one
//! small view differs between them (`loom_analytics.raw_ship_story_points`)
//! and everything downstream — the durable table, the ingest, and all seven
//! PT queries — is shared. That claim is one careless edit away from being
//! false in a way nothing reports: a column renamed on one side only, an
//! attribute key the gateway strips (which yields "nobody sized anything"
//! forever, indistinguishable from an unsized fleet), a failed sweep summed
//! into landed points, or a missing points attribute coerced to a zero-point
//! landing — the exact absent-vs-zero confusion the telemetry schema forbids
//! and this question set exists to prevent.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;

use regex::Regex;

const QUESTIONS: &str =
    include_str!("../../defaults/observability/story-points-throughput-questions.md");
const ROLLUP: &str =
    include_str!("../../defaults/observability/story-points-throughput-rollup.sql");
const QUERIES: &str =
    include_str!("../../defaults/observability/story-points-throughput-queries.sql");
const CLICKSTACK_EXTRACT: &str =
    include_str!("../../defaults/observability/clickstack/story-points-extract.sql");
const SIGNOZ_EXTRACT: &str =
    include_str!("../../defaults/observability/signoz/story-points-extract.sql");
const COLLECTOR_CONFIG: &str = include_str!("../../defaults/observability/collector/config.yaml");
const CLICKSTACK_COMPOSE: &str =
    include_str!("../../defaults/observability/clickstack/compose.yaml");

/// The canonical question IDs. Restated here deliberately: this is the one
/// place the *set* is pinned, and every artifact below is checked against it
/// rather than against another artifact, so two files cannot drift together.
const QUESTION_IDS: &[&str] = &["PT1", "PT2", "PT3", "PT4", "PT5", "PT6", "PT7"];

/// The questions that produce a number a reader may act on (everything but
/// the data-gap and reconciliation questions): each must keep the
/// ship-vs-fail separation visible.
const SUMMING_QUESTIONS: &[&str] = &["PT1", "PT2", "PT3", "PT4", "PT5"];

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
        .find("CREATE TABLE IF NOT EXISTS loom_analytics.ship_story_points")
        .expect("rollup DDL no longer creates ship_story_points");
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
        .find("INSERT INTO loom_analytics.ship_story_points")
        .expect("rollup no longer has an explicit INSERT");
    let body = &ROLLUP[start..];
    let list = &body[body.find('(').unwrap() + 1..body.find(')').unwrap()];
    list.split(',').map(|name| name.trim().to_owned()).collect()
}

/// Keys read out of a String-valued attribute/resource map, in any of the
/// backends' container column names. A key read for its value must survive the
/// gateway, or the query silently reports "nobody sized anything".
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

/// One PT question's query body, cut at the next `-- PTn. ` marker.
fn question_body(id: &str) -> &str {
    let start = QUERIES
        .find(&format!("-- {id}. "))
        .unwrap_or_else(|| panic!("story-points-throughput-queries.sql implements no {id}"));
    let end = QUESTION_IDS
        .iter()
        .filter_map(|next| QUERIES[start + 1..].find(&format!("-- {next}. ")))
        .min()
        .map(|offset| start + 1 + offset)
        .unwrap_or(QUERIES.len());
    &QUERIES[start..end]
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
        "the ClickStack and SigNoz extraction views no longer expose the same per-ship points \
         columns in the same order, so the shared rollup cannot ingest both and cross-backend \
         parity is broken (#8529)"
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
        "ship_story_points's own columns and the extracted ship columns disagree"
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
            "clickstack/story-points-extract.sql",
            CLICKSTACK_EXTRACT,
            vec!["LogAttributes"],
            vec!["ResourceAttributes"],
        ),
        (
            "signoz/story-points-extract.sql",
            SIGNOZ_EXTRACT,
            vec!["attributes_string", "attributes_number"],
            vec!["resources_string"],
        ),
    ] {
        let read = referenced_keys(sql, &signal);
        assert!(
            read.iter().any(|key| key == "loom.story_points"),
            "{label} no longer reads loom.story_points, the whole signal this question set \
             exists for (#9432/#9536)"
        );
        for key in read {
            assert!(
                log_keys.contains(&key),
                "{label} reads attribute '{key}', which the gateway's keep_keys allowlist strips \
                 — the query can only ever report an unsized fleet, which is indistinguishable \
                 from nobody having sized anything"
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
fn the_signoz_extraction_reads_both_attribute_maps_for_the_numeric_key() {
    // `loom.story_points` is emitted as a NUMERIC attribute, and which map
    // SigNoz's pinned ingester files an integer in is exactly the assumption
    // the cycle-time extraction refused to make (#8529). Reading only one map
    // would yield NULL points forever on a pin that chooses the other.
    assert!(
        SIGNOZ_EXTRACT.contains("attributes_number['loom.story_points']")
            && SIGNOZ_EXTRACT.contains("attributes_string['loom.story_points']"),
        "the SigNoz extraction no longer reads loom.story_points from both attributes_number \
         and attributes_string; a numeric-attribute map choice by the ingester would silently \
         turn every sweep unsized"
    );
}

#[test]
fn the_documented_question_set_is_the_implemented_one() {
    for id in QUESTION_IDS {
        assert!(
            QUESTIONS.contains(&format!("**{id}**")),
            "story-points-throughput-questions.md documents no {id}"
        );
        let implementations = QUERIES.matches(&format!("-- {id}. ")).count();
        assert_eq!(
            implementations, 1,
            "story-points-throughput-queries.sql implements {id} {implementations} times; \
             expected exactly once"
        );
    }
    let stray = Regex::new(r"(?m)^-- (PT\d+)\. ").unwrap();
    for capture in stray.captures_iter(QUERIES) {
        assert!(
            QUESTION_IDS.contains(&&capture[1]),
            "story-points-throughput-queries.sql implements {}, which is not in the canonical \
             question set",
            &capture[1]
        );
    }
}

#[test]
fn every_question_binds_its_window_instead_of_hardcoding_one() {
    // Cut on the `-- PTn. ` markers rather than counting occurrences globally:
    // PT7 legitimately binds `{since:DateTime}` twice (once per side of its
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
        "a date literal was written into a story-points-throughput query outside a comment"
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
        "story-points-throughput-questions.md argues about a raw retention of something other \
         than the {raw_ttl} the deployment actually configures"
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
        "story-points-throughput-questions.md claims a rollup retention other than the \
         {rollup_ttl} days the DDL sets"
    );
}

#[test]
fn reads_go_through_the_deduplicating_view_not_the_base_table() {
    // At-least-once delivery means the base table holds duplicate rows; only
    // the `ship_points` view applies FINAL. A question that reads the base
    // table directly would double-count a replayed sweep's points.
    assert!(
        !QUERIES.contains("FROM loom_analytics.ship_story_points"),
        "a story-points-throughput query reads the base table directly; duplicate deliveries \
         would be counted twice — read loom_analytics.ship_points (FINAL) instead"
    );
    assert!(
        ROLLUP.contains("FROM loom_analytics.ship_story_points FINAL"),
        "the ship_points view no longer deduplicates with FINAL"
    );
}

#[test]
fn the_backfill_is_idempotent_over_an_ingested_window() {
    // The dedup contract that makes "re-run the rollup over an already-ingested
    // window" safe: a ReplacingMergeTree keyed on the ship identity, read
    // through FINAL, written by one window-bounded statement.
    assert!(
        ROLLUP.contains("ENGINE = ReplacingMergeTree"),
        "ship_story_points is no longer a ReplacingMergeTree; a re-run backfill would duplicate \
         every ship and double every points-per-day number"
    );
    assert!(
        ROLLUP.contains("ORDER BY (repo, sweep_id, finished_at)"),
        "the ReplacingMergeTree key no longer identifies one ship; replays would no longer \
         collapse"
    );
    let insert = ROLLUP
        .split("INSERT INTO loom_analytics.ship_story_points")
        .nth(1)
        .expect("rollup no longer has an explicit INSERT");
    let insert = &insert[..insert.find(';').unwrap()];
    assert!(
        insert.contains("{since:DateTime}") && insert.contains("{until:DateTime}"),
        "the backfill INSERT is not window-bounded; an unbounded re-run would re-ingest the \
         whole raw table instead of the requested window"
    );
}

#[test]
fn only_successful_ships_land_points() {
    // The ship-vs-fail separation is structural, not conventional: the ONE
    // column any question may sum, points_landed, is NULL on every failed
    // sweep by construction, and the headline question keeps the failed count
    // visible beside the landings so the exclusion is auditable.
    assert!(
        ROLLUP.contains("if(result = 'success', story_points, NULL) AS points_landed"),
        "the dedup view no longer defines points_landed as success-only; a failed sweep (which \
         lands nothing, per the ship definition) could be summed into landed points"
    );
    for id in SUMMING_QUESTIONS {
        assert!(
            question_body(id).contains("result = 'success'"),
            "{id} does not separate successful ships from failed sweeps; a failed sweep lands \
             nothing and must never enter a landed-points answer"
        );
    }
    assert!(
        question_body("PT1").contains("failed_sweeps"),
        "PT1 hides the failed-sweep count; the separation must be visible beside the landings, \
         not just an implicit filter"
    );
}

#[test]
fn missing_points_are_a_data_gap_not_a_zero() {
    // The CT7 discipline: an absent loom.story_points attribute is a data-gap
    // COUNT, never a zero-point landing. Nothing in this set may coerce the
    // absent attribute to 0 — at extraction (OrZero / coalesce) or at
    // aggregation (ifNull(..., 0)) — because that would silently turn an
    // unsized fleet into a zero-throughput one.
    let gap = question_body("PT6");
    assert!(
        gap.contains("points_present") && gap.contains("landings_without_points"),
        "PT6 no longer counts the data gap through points_present; the missing-≠-zero contract \
         has no committed answer"
    );
    for (label, sql) in [
        ("story-points-throughput-queries.sql", QUERIES),
        ("clickstack/story-points-extract.sql", CLICKSTACK_EXTRACT),
        ("signoz/story-points-extract.sql", SIGNOZ_EXTRACT),
    ] {
        assert!(
            !sql.contains("ifNull(story_points, 0)") && !sql.contains("coalesce(story_points, 0"),
            "{label} coerces an absent story_points to 0"
        );
        let or_zero = Regex::new(r"OrZero\([^)]*story_points").unwrap();
        assert!(
            !or_zero.is_match(sql),
            "{label} reads story_points through an OrZero conversion; a missing attribute \
             becomes a measured zero-point landing"
        );
    }
}

#[test]
fn the_reconciliation_query_reads_both_sides_of_the_seam() {
    let body = question_body("PT7");
    assert!(
        body.contains("loom_analytics.raw_ship_story_points"),
        "PT7 no longer reads the raw extraction view; the reconciliation cannot detect drift \
         between the rollup and its source (the CT8 discipline)"
    );
    assert!(
        body.contains("loom_analytics.ship_points"),
        "PT7 no longer reads the rollup; the reconciliation cannot count what the rollup claims"
    );
    assert!(
        body.contains("missing_from_rollup") && body.contains("mismatched_points"),
        "PT7 no longer reports the drift columns; a reconciliation that cannot name the drift \
         it finds is decoration"
    );
}
