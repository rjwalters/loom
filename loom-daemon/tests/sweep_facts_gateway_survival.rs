//! Static contract for the sweep-facts gateway-survival seam (#9586; pattern:
//! `cycle_time_artifacts`, #8665; `loom.story_points` precursor, #9587). No
//! Docker, network, backend or credential is used, so this runs in ordinary
//! CI on any host. Three authorities, all derived rather than restated:
//!
//! 1. every `loom.*` key the sweep-facts extraction SQLs read
//!    (`sweep-facts-extract-{signoz,clickstack}.sql`, SF1..SF8) — parsed from
//!    the SQL text;
//! 2. the daemon's OTLP emission — parsed from the mapping source, so a key
//!    the record carries but never exports fails here too (the #9586
//!    hw_lines/lineage half of the bug);
//! 3. the gateway allowlist — parsed from the collector config the
//!    deployment actually mounts.
//!
//! Why this exists. A key present on the records yet absent from the
//! allowlist yields a silently-NULL extraction column — "the fleet stopped
//! sizing" is indistinguishable from a fleet that never measured (#9546
//! found this for `loom.story_points`; #9586 found eleven more). A key read
//! by the SQL but never emitted is the same NULL by another road. Either
//! failure is invisible until someone notices a rollup going flat, so the
//! build fails instead.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;

use regex::Regex;

const COLLECTOR_CONFIG: &str = include_str!("../../defaults/observability/collector/config.yaml");
const EXTRACTION_SQLS: [&str; 2] = [
    include_str!("../../defaults/observability/sweep-facts/sweep-facts-extract-signoz.sql"),
    include_str!("../../defaults/observability/sweep-facts/sweep-facts-extract-clickstack.sql"),
];
const MAPPING_SOURCES: [&str; 2] = [
    include_str!("../src/observability/otlp/mapping.rs"),
    include_str!("../src/observability/otlp/mapping/metadata.rs"),
];

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

/// Every `loom.*` key any sweep-facts extraction SQL reads.
fn extraction_keys() -> BTreeSet<String> {
    let loom_key = Regex::new(r"loom\.[a-z_]+(\.[a-z_]+)*").unwrap();
    let mut keys = BTreeSet::new();
    for sql in EXTRACTION_SQLS {
        for capture in loom_key.captures_iter(sql) {
            keys.insert(capture[0].to_owned());
        }
    }
    assert!(!keys.is_empty(), "parsed no loom.* keys from the extraction SQLs");
    keys
}

/// Every `loom.*` key the daemon's OTLP mapping exports, derived from the
/// mapping source. Two literal shapes occur, plus one dynamic loop:
/// `format!("loom.config.{key}")` / `format!("loom.{key}")` over
/// `for key in ["runtime", "provider", "configured_model", "arm"]`, expanded
/// here so the derived set stays honest when the loop's list changes.
fn emitted_keys() -> BTreeSet<String> {
    let quoted = Regex::new(r#""([^"]+)""#).unwrap();
    let loom_key = Regex::new(r#"^loom\.[a-z_]+(\.[a-z_]+)*$"#).unwrap();
    let config_loop = Regex::new(r#"for key in \[([^\]]*)\]"#).unwrap();
    let mut keys = BTreeSet::new();
    for source in MAPPING_SOURCES {
        for capture in quoted.captures_iter(source) {
            if loom_key.is_match(&capture[1]) {
                keys.insert(capture[1].to_owned());
            }
        }
    }
    // The dynamic config-key loop: expand both prefixes it emits.
    let loops: Vec<_> = config_loop.captures_iter(MAPPING_SOURCES[1]).collect();
    assert!(
        !loops.is_empty(),
        "the config-key emission loop moved or changed shape; \
                               update this parser to the new source shape"
    );
    for entry in loops {
        for quoted_key in quoted.captures_iter(&entry[1]) {
            let suffix = quoted_key[1].to_owned();
            keys.insert(format!("loom.{suffix}"));
            keys.insert(format!("loom.config.{suffix}"));
        }
    }
    assert!(!keys.is_empty(), "parsed no loom.* keys from the mapping source");
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

/// Every key the extraction reads must be EXPORTED by the daemon — a read of
/// a never-emitted key is a NULL column by another road (#9586).
#[test]
fn every_sweep_facts_extraction_key_is_emitted_by_the_daemon() {
    let emitted = emitted_keys();
    let read = extraction_keys();
    let missing: Vec<String> = read.difference(&emitted).cloned().collect();
    assert!(
        missing.is_empty(),
        "the sweep-facts extraction SQLs read loom.* keys the OTLP mapping never \
         exports: {missing:?} — either export them (absent-not-zero) or stop reading \
         them; a silent NULL column is not an option"
    );
}

/// Every key the extraction reads must SURVIVE the gateway's log allowlist —
/// `keep_keys` deletes everything not named, so an unadmitted emitted key is
/// a silently-NULL column (#9546 was exactly this for loom.story_points).
#[test]
fn every_sweep_facts_extraction_key_survives_the_gateway_log_allowlist() {
    let keys = allowlist("log");
    let read = extraction_keys();
    let missing: Vec<String> = read.difference(&keys).cloned().collect();
    assert!(
        missing.is_empty(),
        "the gateway's log keep_keys strips loom.* keys the sweep-facts extraction \
         SQLs read: {missing:?} — the columns extract as silent NULLs on the mounted \
         deployment, indistinguishable from a fleet that never measured"
    );
}
