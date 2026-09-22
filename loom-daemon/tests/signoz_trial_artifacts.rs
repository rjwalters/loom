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

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use regex::Regex;

const COLLECTOR_CONFIG: &str = include_str!("../../defaults/observability/collector/config.yaml");
const FIXTURE_QUERIES: &str =
    include_str!("../../defaults/observability/signoz/fixture-queries.sql");
/// The original ad-hoc deployment proof. Its trace ID and gauge predate the
/// shared manifest, so only the key-forwarding rules apply to it — not the
/// manifest-equality rules that govern `fixture-queries.sql`.
const ADHOC_QUERIES: &str = include_str!("../../defaults/observability/signoz/queries.sql");
const SIGNOZ_README: &str = include_str!("../../defaults/observability/signoz/README.md");

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
