//! Static contract: the SigNoz trial's saved query artifacts must only reference
//! telemetry vocabulary that Loom actually exports and that the neutral gateway
//! actually forwards. No Docker, network, backend, or credential is used, so this
//! runs in ordinary CI on any host.
//!
//! Why this exists (#8528): the SigNoz trial artifacts landed before the shared
//! fixture generator (#8578) and the owned lifecycle span enumeration (#8579), so
//! their query vocabulary was written against an ad-hoc single-trace probe and
//! could not answer the shared manifest's questions at all. A saved query that
//! subscripts an attribute the gateway strips, or filters on a span name Loom
//! never emits, does not fail — it silently returns zero rows, which is
//! indistinguishable from "the backend lost the data". That is precisely the
//! confusion the trial exists to rule out, so the drift is asserted here instead
//! of being re-discovered by hand on the trial host.
//!
//! The authorities are derived, never restated: span/metric/attribute vocabulary
//! comes from the generated fixture manifest, and the forwarding allowlist comes
//! from the gateway config the deployment actually mounts.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use loom_daemon::telemetry::ci::{
    CiAttr, CI_JOB_DURATION_METRIC, CI_METRIC_LABEL_KEYS, CI_RUN_DURATION_METRIC,
};
use loom_daemon::telemetry::{CiJobLogRecord, CiJobRecord, CiRunRecord, RepoVisibility};
use regex::Regex;

const COLLECTOR_CONFIG: &str = include_str!("../../defaults/observability/collector/config.yaml");
const FIXTURE_QUERIES: &str =
    include_str!("../../defaults/observability/signoz/fixture-queries.sql");
/// The original ad-hoc deployment proof. Its trace ID and gauge predate the
/// shared manifest, so only the key-forwarding rules apply to it — not the
/// manifest-equality rules that govern `fixture-queries.sql`.
const ADHOC_QUERIES: &str = include_str!("../../defaults/observability/signoz/queries.sql");
const SIGNOZ_README: &str = include_str!("../../defaults/observability/signoz/README.md");
/// The standing build/CI retro queries (#8826). Governed by the CI record
/// family's own vocabulary, not the shared fixture manifest's.
const CI_QUERIES: &str = include_str!("../../defaults/observability/signoz/ci-queries.sql");

/// Loom's own attribute namespace. Presence probes outside it are deliberate
/// absence assertions (see [`fixture_queries_assert_the_privacy_sentinel_is_dropped`])
/// rather than reads that need the gateway to forward anything.
const LOOM_NAMESPACE: &str = "loom.";

/// Keys the gateway's `transform/privacy` processor keeps, split by OTTL context.
/// Anything outside these sets never reaches either backend.
struct Allowlist {
    resource: BTreeSet<String>,
    signal: BTreeSet<String>,
}

/// Walks the collector config line by line, tagging every `keep_keys(...)` list
/// with the most recent `- context:` so resource keys stay distinct from
/// span/log/datapoint keys.
fn allowlist() -> Allowlist {
    let quoted = Regex::new(r#""([^"]+)""#).unwrap();
    let mut out = Allowlist {
        resource: BTreeSet::new(),
        signal: BTreeSet::new(),
    };
    let mut context = String::new();
    for line in COLLECTOR_CONFIG.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- context:") {
            context = rest.trim().to_owned();
        }
        if !trimmed.contains("keep_keys(") {
            continue;
        }
        let target = if context == "resource" {
            &mut out.resource
        } else {
            &mut out.signal
        };
        for capture in quoted.captures_iter(trimmed) {
            target.insert(capture[1].to_owned());
        }
    }
    assert!(
        out.resource.contains("host.id"),
        "collector config no longer keeps host.id; the fixture isolates a run by that resource key"
    );
    assert!(
        !out.resource.is_empty() && !out.signal.is_empty(),
        "failed to parse keep_keys allowlists out of the collector config"
    );
    out
}

/// Keys read for their **value**. A stripped key here yields an empty string or
/// no rows, so every one of these must survive the gateway.
fn subscript_keys(sql: &str, container: &str) -> BTreeSet<String> {
    Regex::new(&format!(r"{container}\['([^']+)'\]"))
        .unwrap()
        .captures_iter(sql)
        .map(|capture| capture[1].to_owned())
        .collect()
}

/// Keys probed for **presence** via `mapContains`, restricted to Loom's own
/// namespace: a probe for a prohibited foreign key is an absence assertion.
fn contains_keys(sql: &str, container: &str) -> BTreeSet<String> {
    Regex::new(&format!(r"mapContains\(\s*{container}\s*,\s*'([^']+)'\s*\)"))
        .unwrap()
        .captures_iter(sql)
        .map(|capture| capture[1].to_owned())
        .filter(|key| key.starts_with(LOOM_NAMESPACE))
        .collect()
}

/// Every Loom attribute key the artifact depends on, however it is accessed.
fn referenced_attribute_keys(sql: &str, container: &str) -> BTreeSet<String> {
    let mut keys = subscript_keys(sql, container);
    keys.extend(contains_keys(sql, container));
    keys
}

/// Span-name literals in Loom's namespace. Anchored on a non-identifier byte so
/// `metric_name = '…'` is not mistaken for a span name, and namespace-filtered so
/// a `system.columns` predicate such as `name = 'labels'` is not either.
fn span_name_literals(sql: &str) -> BTreeSet<String> {
    Regex::new(r"(?:^|[^0-9A-Za-z_])name\s*=\s*'([^']+)'")
        .unwrap()
        .captures_iter(sql)
        .map(|capture| capture[1].to_owned())
        .filter(|name| name.starts_with(LOOM_NAMESPACE))
        .collect()
}

/// Metric-name literals from `metric_name IN ('a', 'b')` lists. `LIKE` patterns
/// are prefixes for schema discovery, not identity claims, and are ignored.
fn metric_name_literals(sql: &str) -> BTreeSet<String> {
    let list = Regex::new(r"metric_name\s+IN\s*\(([^)]*)\)").unwrap();
    let quoted = Regex::new(r"'([^']+)'").unwrap();
    list.captures_iter(sql)
        .flat_map(|capture| {
            quoted
                .captures_iter(capture.get(1).unwrap().as_str())
                .map(|inner| inner[1].to_owned())
                .collect::<Vec<_>>()
        })
        .collect()
}

struct Manifest {
    span_names: BTreeSet<String>,
    metric_names: BTreeSet<String>,
    span_attributes: BTreeSet<String>,
    /// `expected_distinct` as `(spans, logs, metric_data_points)` — the counts a
    /// reader compares an observation against.
    expected_distinct: (u64, u64, u64),
}

/// The generated shared fixture is the authority on what a query can match.
fn manifest() -> Manifest {
    let start: DateTime<Utc> = "2026-09-21T12:00:00Z".parse().unwrap();
    let bundle = loom_daemon::telemetry::fixture::build("artifact-contract", start).unwrap();
    let distinct = &bundle.manifest["expected_distinct"];
    let expected_distinct = (
        distinct["spans"].as_u64().unwrap(),
        distinct["logs"].as_u64().unwrap(),
        distinct["metric_data_points"].as_u64().unwrap(),
    );
    let spans = bundle.manifest["spans"].as_array().unwrap();
    let mut span_attributes = BTreeSet::new();
    for span in spans {
        for key in span["attributes"].as_object().unwrap().keys() {
            span_attributes.insert(key.clone());
        }
    }
    Manifest {
        span_names: spans
            .iter()
            .map(|span| span["name"].as_str().unwrap().to_owned())
            .collect(),
        metric_names: bundle.manifest["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|metric| metric["name"].as_str().unwrap().to_owned())
            .collect(),
        span_attributes,
        expected_distinct,
    }
}

/// Strips SQL line-comment markers and markdown emphasis, then collapses runs of
/// whitespace, so a sentence wrapped across several comment lines matches as one.
fn flatten_prose(text: &str) -> String {
    let stripped: String = text
        .lines()
        .map(|line| line.trim_start().trim_start_matches("--").replace('*', " "))
        .collect::<Vec<_>>()
        .join(" ");
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The counts each artifact tells a reader to expect, in manifest order.
fn quoted_distinct_counts(text: &str) -> Option<(u64, u64, u64)> {
    let claim =
        Regex::new(r"(\d+) distinct spans, (\d+) correlated logs and (\d+) metric data points")
            .unwrap();
    let flattened = flatten_prose(text);
    let captured = claim.captures(&flattened)?;
    Some((
        captured[1].parse().unwrap(),
        captured[2].parse().unwrap(),
        captured[3].parse().unwrap(),
    ))
}

#[test]
fn saved_queries_only_reference_forwarded_attribute_and_resource_keys() {
    let allowed = allowlist();
    for (label, sql) in [
        ("fixture-queries.sql", FIXTURE_QUERIES),
        ("queries.sql", ADHOC_QUERIES),
        ("ci-queries.sql", CI_QUERIES),
    ] {
        for container in ["attributes_string", "attributes_number", "attributes_bool"] {
            for key in referenced_attribute_keys(sql, container) {
                assert!(
                    allowed.signal.contains(&key),
                    "{label}: {container}['{key}'] is stripped by the gateway's keep_keys \
                     allowlist, so this query can only ever return zero rows"
                );
            }
        }
        for key in subscript_keys(sql, "resources_string") {
            assert!(
                allowed.resource.contains(&key),
                "{label}: resources_string['{key}'] is not kept by the gateway's resource \
                 allowlist, so this query can only ever return zero rows"
            );
        }
    }
}

#[test]
fn fixture_queries_match_the_generated_manifest_vocabulary() {
    let expected = manifest();

    let names = span_name_literals(FIXTURE_QUERIES);
    assert!(!names.is_empty(), "fixture-queries.sql no longer filters on any span name");
    for name in &names {
        assert!(
            expected.span_names.contains(name),
            "fixture-queries.sql filters span name '{name}', which the shared fixture never \
             emits; emitted names are {:?}",
            expected.span_names
        );
    }

    assert_eq!(
        metric_name_literals(FIXTURE_QUERIES),
        expected.metric_names,
        "fixture-queries.sql must query exactly the shared fixture's metric names"
    );

    for key in referenced_attribute_keys(FIXTURE_QUERIES, "attributes_string") {
        assert!(
            expected.span_attributes.contains(&key),
            "fixture-queries.sql reads span attribute '{key}', which the shared fixture never \
             sets; emitted attributes are {:?}",
            expected.span_attributes
        );
    }
}

/// A count is the only thing separating "the backend indexed everything" from
/// "the backend silently dropped rows", so a stale one is worse than none: query
/// 1 returns a number, and a reader compares it against whatever the surrounding
/// prose claims. Both artifacts must therefore quote the manifest's own
/// `expected_distinct`, not a figure that was true for an earlier fixture version.
#[test]
fn saved_artifacts_quote_the_manifests_expected_distinct_counts() {
    let expected = manifest().expected_distinct;
    for (label, text) in [
        ("fixture-queries.sql", FIXTURE_QUERIES),
        ("README.md", SIGNOZ_README),
    ] {
        let quoted = quoted_distinct_counts(text).unwrap_or_else(|| {
            panic!(
                "{label} no longer states the expected span/log/metric counts a reader compares \
                 an observed total against"
            )
        });
        assert_eq!(
            quoted, expected,
            "{label} quotes {quoted:?} distinct (spans, logs, metric data points), but the \
             generated fixture manifest now expects {expected:?}"
        );
    }
}

#[test]
fn fixture_queries_assert_the_privacy_sentinel_is_dropped() {
    let sentinel = loom_daemon::telemetry::fixture::PRIVACY_SENTINEL;
    assert!(
        FIXTURE_QUERIES.contains(sentinel),
        "fixture-queries.sql must search all three signals for the fixture's privacy sentinel"
    );
    let allowed = allowlist();
    // The sentinel rides on this prohibited attribute; the assertion in
    // fixture-queries.sql is only meaningful while the gateway drops the key.
    assert!(
        !allowed.signal.contains("prompt.content") && !allowed.resource.contains("prompt.content"),
        "the gateway allowlist now keeps prompt.content, which would forward prompt text"
    );
    assert!(
        FIXTURE_QUERIES.contains("mapContains(attributes_string, 'prompt.content')"),
        "fixture-queries.sql must check for the prohibited attribute itself, not only its value"
    );
}

#[test]
fn readme_documents_the_gateway_endpoint_the_collector_config_exports_to() {
    let endpoint = Regex::new(r"otlp_http/signoz:\s*\n\s*endpoint:\s*(\S+)")
        .unwrap()
        .captures(COLLECTOR_CONFIG)
        .expect("collector config must define the otlp_http/signoz exporter endpoint")
        .get(1)
        .unwrap()
        .as_str()
        .to_owned();
    assert!(
        SIGNOZ_README.contains(&endpoint),
        "the SigNoz README must document the gateway's actual SigNoz exporter endpoint {endpoint}"
    );
}

// ============================================================================
// ci-queries.sql (#8826): the standing build/CI retro queries
// ============================================================================
//
// The CI record family has its own authorities, so it gets its own, stricter
// drift guard. A CI query can go silently empty in three ways the shared
// `allowlist()` check cannot see:
//
// 1. It reads a log attribute the gateway's **log** `keep_keys` strips (the
//    shared check unions log and span keys, so a span-only key would pass).
// 2. It reads a key from the wrong SigNoz type container — e.g.
//    `attributes_string['loom.ci.run_id']`, when the daemon sends the run id as
//    an OTLP int that SigNoz files under `attributes_number`. The subscript
//    returns '' for every row and nothing errors.
// 3. It reads a metric label or names a histogram series the daemon never
//    emits or the gateway's **datapoint** `keep_keys` strips.

/// `keep_keys` per OTTL context (`log`, `span`, `spanevent`, `datapoint`,
/// `resource`), so a CI log query is checked against the log allowlist alone.
fn keep_keys_by_context() -> BTreeMap<String, BTreeSet<String>> {
    let quoted = Regex::new(r#""([^"]+)""#).unwrap();
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut context = String::new();
    for line in COLLECTOR_CONFIG.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- context:") {
            context = rest.trim().to_owned();
        }
        if !trimmed.contains("keep_keys(") {
            continue;
        }
        let keys = out.entry(context.clone()).or_default();
        for capture in quoted.captures_iter(trimmed) {
            keys.insert(capture[1].to_owned());
        }
    }
    for context in ["log", "datapoint"] {
        assert!(
            out.get(context).is_some_and(|keys| !keys.is_empty()),
            "failed to parse the `{context}` keep_keys allowlist out of the collector config"
        );
    }
    out
}

/// The SigNoz map column each CI log attribute lands in, re-derived from the
/// daemon's own record rendering (fully-populated records, so every optional
/// key is present): an OTLP string lands in `attributes_string`, an int in
/// `attributes_number`, a bool in `attributes_bool`. `loom.repo` and
/// `loom.repo.visibility` are prepended to every CI log record as strings by
/// the OTLP mapping.
fn daemon_ci_log_attribute_containers() -> BTreeMap<String, &'static str> {
    let at: DateTime<Utc> = "2026-09-25T12:00:00Z".parse().unwrap();
    let run = CiRunRecord {
        repo: "2amlogic/example".into(),
        visibility: RepoVisibility::Private,
        run_id: 1,
        run_attempt: 1,
        workflow: "CI".into(),
        git_ref: Some("main".into()),
        head_sha: "0".repeat(40),
        event: "push".into(),
        status: "completed".into(),
        conclusion: Some("failure".into()),
        triggered_by: Some("octocat".into()),
        started_at: at,
        completed_at: at,
        duration_ms: 0,
        queued_ms: Some(0),
    };
    let job = CiJobRecord {
        repo: "2amlogic/example".into(),
        visibility: RepoVisibility::Private,
        run_id: 1,
        job_id: 2,
        workflow: "CI".into(),
        job: "build".into(),
        runner: Some("ubuntu-latest".into()),
        attempts: 1,
        status: "completed".into(),
        conclusion: Some("failure".into()),
        timed_out: false,
        started_at: at,
        completed_at: at,
        duration_ms: 0,
    };
    let chunk = CiJobLogRecord {
        repo: "2amlogic/example".into(),
        visibility: RepoVisibility::Private,
        run_id: 1,
        job_id: 2,
        workflow: "CI".into(),
        job: "build".into(),
        chunk_index: 0,
        chunk_count: 1,
        log_bytes_total: 0,
        truncated: true,
        truncation_note: Some("capped".into()),
        completed_at: at,
        text: String::new(),
    };
    let mut out = BTreeMap::from([
        ("loom.repo".to_owned(), "attributes_string"),
        ("loom.repo.visibility".to_owned(), "attributes_string"),
    ]);
    for (key, value) in run
        .log_attributes()
        .into_iter()
        .chain(job.log_attributes())
        .chain(chunk.log_attributes())
    {
        let container = match value {
            CiAttr::Str(_) => "attributes_string",
            CiAttr::Int(_) => "attributes_number",
            CiAttr::Bool(_) => "attributes_bool",
        };
        let previous = out.insert(key.to_owned(), container);
        assert!(
            previous.is_none_or(|p| p == container),
            "the daemon renders CI attribute {key} as two different OTLP types across record \
             kinds, so no single SigNoz column can be queried for it"
        );
    }
    out
}

/// Metric labels read out of a series' JSON `labels` column.
fn metric_label_keys(sql: &str) -> BTreeSet<String> {
    Regex::new(r"JSONExtractString\(\s*(?:\w+\.)?labels\s*,\s*'([^']+)'\s*\)")
        .unwrap()
        .captures_iter(sql)
        .map(|capture| capture[1].to_owned())
        .collect()
}

/// Every metric-name literal, whether `metric_name = '…'` or an `IN` list.
fn all_metric_name_literals(sql: &str) -> BTreeSet<String> {
    let mut names = metric_name_literals(sql);
    names.extend(
        Regex::new(r"metric_name\s*=\s*'([^']+)'")
            .unwrap()
            .captures_iter(sql)
            .map(|capture| capture[1].to_owned()),
    );
    names
}

#[test]
fn ci_queries_read_log_attributes_the_gateway_forwards_from_the_column_the_daemon_fills() {
    let log_keys = &keep_keys_by_context()["log"];
    let daemon = daemon_ci_log_attribute_containers();
    for container in ["attributes_string", "attributes_number", "attributes_bool"] {
        let keys = referenced_attribute_keys(CI_QUERIES, container);
        assert!(
            !keys.is_empty(),
            "ci-queries.sql no longer reads anything from {container}; if that is deliberate, \
             drop it from this loop rather than letting the guard go vacuous"
        );
        for key in keys {
            assert!(
                log_keys.contains(&key),
                "ci-queries.sql: {container}['{key}'] is stripped by the gateway's LOG keep_keys \
                 allowlist, so this query can only ever return zero rows"
            );
            match daemon.get(&key) {
                None => panic!(
                    "ci-queries.sql reads {container}['{key}'], which no ci.run / ci.job / \
                     ci.job.log record ever sets; the daemon's CI log vocabulary is {:?}",
                    daemon.keys().collect::<Vec<_>>()
                ),
                Some(actual) => assert_eq!(
                    *actual, container,
                    "ci-queries.sql reads '{key}' from {container}, but the daemon sends it as the \
                     type SigNoz files under {actual} — the subscript returns an empty/zero value \
                     for every row instead of an error"
                ),
            }
        }
    }
}

#[test]
fn ci_queries_read_only_metric_labels_the_daemon_emits_and_the_gateway_forwards() {
    let datapoint_keys = &keep_keys_by_context()["datapoint"];
    let labels = metric_label_keys(CI_QUERIES);
    assert!(!labels.is_empty(), "ci-queries.sql no longer reads any CI metric label");
    for label in labels {
        assert!(
            CI_METRIC_LABEL_KEYS.contains(&label.as_str()),
            "ci-queries.sql reads metric label '{label}', which the CI duration histograms never \
             carry; their labels are {CI_METRIC_LABEL_KEYS:?}"
        );
        assert!(
            datapoint_keys.contains(&label),
            "ci-queries.sql reads metric label '{label}', which the gateway's DATAPOINT \
             keep_keys allowlist strips"
        );
    }
}

/// SigNoz stores one OTLP histogram as `<name>.bucket` / `.count` / `.sum`
/// series (observed live on the pinned v0.142.1 ingester). The retro reads
/// only `.sum` (one sample = one observed duration, since every data point has
/// count 1) and `.count` (one sample = one run/job).
#[test]
fn ci_queries_name_only_the_ci_duration_histogram_series() {
    let allowed: BTreeSet<String> = [CI_RUN_DURATION_METRIC, CI_JOB_DURATION_METRIC]
        .iter()
        .flat_map(|metric| [format!("{metric}.sum"), format!("{metric}.count")])
        .collect();
    let named = all_metric_name_literals(CI_QUERIES);
    for name in &named {
        assert!(
            allowed.contains(name),
            "ci-queries.sql queries metric series '{name}', which no CI duration histogram \
             produces; expected one of {allowed:?}"
        );
    }
    for metric in [CI_RUN_DURATION_METRIC, CI_JOB_DURATION_METRIC] {
        assert!(
            named.iter().any(|name| name.starts_with(metric)),
            "ci-queries.sql no longer reads {metric} at all"
        );
    }
}

/// `clickhouse-client` refuses a statement with an unbound `{param:Type}`, and
/// a documented-but-unused `--param_` is a stale instruction. The header's
/// invocation must bind exactly the parameters the statements use.
#[test]
fn ci_queries_documented_invocation_binds_exactly_the_parameters_used() {
    let used: BTreeSet<String> = Regex::new(r"\{(\w+):\w+\}")
        .unwrap()
        .captures_iter(CI_QUERIES)
        .map(|capture| capture[1].to_owned())
        .collect();
    let bound: BTreeSet<String> = Regex::new(r"--param_(\w+)=")
        .unwrap()
        .captures_iter(CI_QUERIES)
        .map(|capture| capture[1].to_owned())
        .collect();
    assert!(!used.is_empty(), "ci-queries.sql is no longer parameterized");
    assert_eq!(
        used, bound,
        "ci-queries.sql's documented clickhouse-client invocation must bind exactly the \
         parameters its statements reference"
    );
}

/// The six standing views are the issue's contract (#8826): each is a numbered
/// section of `ci-queries.sql`, and each has a reproducible row in the
/// README's "Saved views" table that cites it.
#[test]
fn every_standing_ci_view_is_a_query_section_and_a_readme_saved_view() {
    for section in 1..=6 {
        assert!(
            Regex::new(&format!(r"(?m)^-- {section}\. "))
                .unwrap()
                .is_match(CI_QUERIES),
            "ci-queries.sql is missing standing-view section {section}"
        );
        assert!(
            SIGNOZ_README.contains(&format!("`ci-queries.sql` {section})")),
            "the SigNoz README's Saved views table has no row citing `ci-queries.sql` {section}"
        );
    }
}
