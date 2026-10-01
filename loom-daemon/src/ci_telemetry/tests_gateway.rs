//! The AC7 gateway-vocabulary contract, extracted verbatim from
//! `tests.rs` (#9764: that file sat exactly at the file-size ratchet's
//! 1000-code-line threshold, and scoping the scrub-class test to its kind's
//! marker pushed it over). A child module of `tests`, so `super::*` reaches
//! every fixture and helper unchanged.

use std::collections::BTreeSet;

use super::*;

const COLLECTOR_CONFIG: &str =
    include_str!("../../../defaults/observability/collector/config.yaml");

/// The quoted keys of the `keep_keys` list under `- context: <context>`.
pub(super) fn keep_keys(context: &str) -> BTreeSet<String> {
    let mut current = String::new();
    let mut keys = BTreeSet::new();
    for line in COLLECTOR_CONFIG.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- context:") {
            current = rest.trim().to_string();
        }
        if current == context && trimmed.contains("keep_keys(") {
            keys.extend(trimmed.split('"').skip(1).step_by(2).map(str::to_string));
        }
    }
    keys
}

fn ci_keys(keys: &BTreeSet<String>) -> BTreeSet<String> {
    keys.iter()
        .filter(|k| k.starts_with("loom.ci."))
        .cloned()
        .collect()
}

fn set(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|k| (*k).to_string()).collect()
}

#[test]
fn collector_allowlist_matches_the_ci_vocabulary_exactly() {
    assert_eq!(ci_keys(&keep_keys("log")), set(CI_LOG_ATTRIBUTE_KEYS));
    assert_eq!(ci_keys(&keep_keys("span")), set(CI_SPAN_ATTRIBUTE_KEYS));
    let datapoint = keep_keys("datapoint");
    for label in CI_METRIC_LABEL_KEYS {
        assert!(datapoint.contains(*label), "datapoint keep_keys lacks {label}");
    }
    for shared in ["loom.repo", "loom.repo.visibility"] {
        assert!(keep_keys("log").contains(shared) && keep_keys("span").contains(shared));
    }
    // #8825: the gateway's scrub stage is scoped by this one key. If the
    // allowlist ever dropped it, ingest would still succeed but a job log
    // would become unreconstructable (chunk ordering is the only contract).
    assert!(keep_keys("log").contains(crate::telemetry::ci::CI_LOG_CHUNK_MARKER_KEY));
    assert!(CI_LOG_ATTRIBUTE_KEYS.contains(&crate::telemetry::ci::CI_LOG_CHUNK_MARKER_KEY));
}

/// The scrub-class list in the collector config and `CI_LOG_SCRUB_CLASSES`
/// must agree, in order — the daemon-side half of #8825's "the list lives in
/// the repo, reviewable" rule (the integration test
/// `collector_fanout::gateway_scrubs_exactly_the_declared_ci_log_classes`
/// asserts the same from the other side, including the scope guards). Since
/// #9764 the config carries a second body-rewriting stage
/// (`transform/session_output_redaction`) with the same class list under its
/// own marker, so this test — and the sibling in
/// `activity::transcript_output`'s tests — filters to the statements scoped
/// by THIS kind's marker; neither stage may drift from the shared list.
#[test]
fn collector_scrub_classes_match_the_declared_list() {
    // A class may need more than one pattern (github-token covers both the
    // `gh*_` prefixes and `github_pat_`), so consecutive repeats collapse —
    // but the ORDER of classes is load-bearing and is compared exactly.
    let mut markers: Vec<String> = Vec::new();
    for line in COLLECTOR_CONFIG
        .lines()
        .filter(|line| line.contains("replace_pattern(body,"))
        .filter(|line| {
            line.contains(&format!(
                "attributes[\"{}\"] != nil",
                crate::telemetry::ci::CI_LOG_CHUNK_MARKER_KEY
            ))
        })
    {
        let start = line.find("[REDACTED:").expect("a replacement marker");
        let end = line[start..].find(']').expect("a closed marker") + start;
        let class = line[start + "[REDACTED:".len()..end].to_string();
        if markers.last() != Some(&class) {
            markers.push(class);
        }
    }
    assert_eq!(
        markers,
        crate::telemetry::ci::CI_LOG_SCRUB_CLASSES
            .iter()
            .map(|c| (*c).to_string())
            .collect::<Vec<_>>()
    );
    for class in crate::telemetry::ci::CI_LOG_SCRUB_CLASSES {
        assert_eq!(crate::telemetry::ci::scrub_marker(class), format!("[REDACTED:{class}]"));
    }
}

#[test]
fn records_emit_exactly_the_declared_vocabulary() {
    let dir = TempDir::new().unwrap();
    // Capture on, so `ci.job.log`'s keys (including the truncation marker's)
    // are in the union too — the fixture exercises every optional field.
    run_cycle(&ctx_with_logs(dir.path()), &FixtureApi::new()).unwrap();
    let (mut log_keys, mut span_keys, mut labels) =
        (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    for env in journal(dir.path()) {
        match env.record {
            TelemetryRecord::CiRun(r) => {
                log_keys.extend(r.log_attributes().into_iter().map(|(k, _)| k.to_string()))
            }
            TelemetryRecord::CiJob(r) => {
                log_keys.extend(r.log_attributes().into_iter().map(|(k, _)| k.to_string()))
            }
            TelemetryRecord::CiJobLog(r) => {
                log_keys.extend(r.log_attributes().into_iter().map(|(k, _)| k.to_string()))
            }
            TelemetryRecord::CiDuration(r) => {
                labels.extend(r.metric_labels().into_iter().map(|(k, _)| k.to_string()))
            }
            TelemetryRecord::Span(s) => {
                span_keys.extend(ci_keys(&s.attributes.keys().cloned().collect()))
            }
            _ => {}
        }
    }
    // The fixture exercises every optional field, so the union is the full set.
    assert_eq!(log_keys, set(CI_LOG_ATTRIBUTE_KEYS));
    assert_eq!(span_keys, set(CI_SPAN_ATTRIBUTE_KEYS));
    assert_eq!(labels, set(CI_METRIC_LABEL_KEYS));
}
