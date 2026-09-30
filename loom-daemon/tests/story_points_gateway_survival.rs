//! Static contract for the `loom.story_points` gateway-survival seam (follow-up
//! to #9536 and the curated #9433 curation, epic #9429; pattern:
//! `cycle_time_artifacts`, #8665). No Docker, network, backend or credential
//! is used, so this runs in ordinary CI on any host. The authorities are
//! derived rather than restated: the forwarding allowlist is parsed from the
//! collector config the deployment actually mounts, and the attribute keys are
//! read out of the extraction SQL that consumes them.
//!
//! Why this exists. `sweep.started` / `sweep.outcome` carry
//! `loom.story_points` (#9432), and the merged sweep-facts extraction
//! (`sweep-facts-extract-{signoz,clickstack}.sql`, SF8) reads it off the OTLP
//! path — but the gateway forwards only what its `keep_keys` allowlist admits.
//! A key present in the records yet absent from the allowlist yields "nobody
//! sized anything" forever, indistinguishable from an unsized fleet: the exact
//! absent-vs-zero confusion the telemetry schema forbids. Nothing else on main
//! asserted this seam, so one dropped key failed silently.
//!
//! Scope note: the sweep-facts extractions read OTHER `loom.*` keys the
//! allowlist currently strips (`loom.attempt_index`, `loom.disposition`,
//! `loom.rework_events`, `loom.hw_lines_*`) — a pre-existing gap tracked
//! separately; this file pins only the seam this epic introduced.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;

use regex::Regex;

const COLLECTOR_CONFIG: &str = include_str!("../../defaults/observability/collector/config.yaml");

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
fn loom_story_points_survives_the_gateway_log_allowlist() {
    let keys = allowlist("log");
    assert!(
        keys.contains("loom.story_points"),
        "the gateway's log keep_keys strips `loom.story_points`: #9432 emits it and the \
         merged sweep-facts extraction reads it, so behind this gateway every sweep reads \
         as unsized (NULL forever, indistinguishable from an unsized fleet)"
    );
}
