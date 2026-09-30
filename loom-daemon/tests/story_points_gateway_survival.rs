//! Static contract for the `loom.story_points` gateway-survival seam (follow-up
//! to #9536 and the curated #9433 curation, epic #9429; pattern:
//! `cycle_time_artifacts`, #8665) — and, since #9586, for EVERY `loom.*` key
//! the sweep-facts extraction views read. No Docker, network, backend or
//! credential is used, so this runs in ordinary CI on any host. The
//! authorities are derived rather than restated: the forwarding allowlist is
//! parsed from the collector config the deployment actually mounts, and the
//! attribute keys are read out of the extraction SQL that consumes them.
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
//! Scope note (#9586): the guard is now GENERAL. Every `loom.*` key the
//! sweep-facts extraction views read is checked against the allowlist: either
//! the daemon exports it as a `sweep.outcome` OTLP log attribute (the emission
//! surface is `observability/otlp/mapping.rs` + `mapping/metadata.rs`) and the
//! key must be ADMITTED, or the daemon does not export it as a log attribute
//! (it rides the D1/JSONL payload, and for the three lineage keys span
//! metadata only) and it must be PINNED OTLP-absent below — admitting a key
//! nothing sends is noise, and the pin keeps that extraction column's
//! backend-side NULL a documented contract instead of a silent one.
//! `loom.disposition` and `loom.tokens_unattributed_{in,out}` were the
//! emitted-but-stripped keys #9586 admitted; the pinned list is the residue.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;

use regex::Regex;

const COLLECTOR_CONFIG: &str = include_str!("../../defaults/observability/collector/config.yaml");
const CLICKSTACK_EXTRACT: &str =
    include_str!("../../defaults/observability/sweep-facts/sweep-facts-extract-clickstack.sql");
const SIGNOZ_EXTRACT: &str =
    include_str!("../../defaults/observability/sweep-facts/sweep-facts-extract-signoz.sql");

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

/// Every `loom.*` attribute key an extraction SQL READS, taken from its
/// executable lines only — the headers of both files name keys in prose (they
/// document this very gap), and a read is a subscript or a `mapContains`
/// probe, both of which spell the key as a `loom.` token.
fn loom_attribute_reads(sql: &str) -> BTreeSet<String> {
    let key = Regex::new(r"loom\.[a-z_][a-z0-9_]*(?:\.[a-z_][a-z0-9_]*)*").unwrap();
    sql.lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .flat_map(|line| {
            key.find_iter(line)
                .map(|found| found.as_str().to_owned())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The per-key OTLP-absence decision (#9586). Each of these keys is READ by
/// the sweep-facts extraction views but NOT exported by the daemon as a
/// `sweep.outcome` OTLP LOG attribute — the emission surface is
/// `observability/otlp/mapping.rs` + `mapping/metadata.rs`, and none of these
/// keys appear there. Three ride trace-span metadata only
/// (`sweep_registry/outcome_journal.rs`); the rest exist only in the
/// D1/JSONL record payload, which the D1 rollup reads with `json_extract`.
///
/// Admitting any of them in the gateway's `keep_keys` would forward nothing
/// (a dead allowlist entry is noise), and dropping the columns would break
/// the shared fact shape `sweep_facts_artifacts.rs` enforces — D1
/// legitimately fills them from the payload. So the column is GATED instead:
/// pinned here, which turns its ClickHouse-side NULL from a silent loss into
/// a documented contract the build owns. To retire an entry: export the field
/// as an OTLP log attribute in `metadata::outcome`, admit the key in
/// `defaults/observability/collector/config.yaml`, and delete the entry —
/// `every_sweep_facts_attribute_read_survives_the_gateway_or_is_pinned_absent`
/// then holds the key to the same standard as `loom.story_points`.
const OTLP_LOG_ABSENT: &[(&str, &str)] = &[
    (
        "loom.attempt_index",
        "#9444 lineage counter; span metadata (outcome_journal.rs) and the D1 payload only",
    ),
    (
        "loom.previous_sweep_id",
        "#9444 lineage; span metadata (outcome_journal.rs) and the D1 payload only",
    ),
    (
        "loom.trigger",
        "#9444 dispatch trigger; span metadata (outcome_journal.rs) and the D1 payload only",
    ),
    ("loom.rework_events", "#9444 rework timeline; D1 payload only ($.rework_events)"),
    ("loom.pr_numbers", "#9465 multi-PR slice; D1 payload only"),
    ("loom.hw_lines_added", "#9466 hand-written diff slice; D1 payload only"),
    ("loom.hw_lines_deleted", "#9466 hand-written diff slice; D1 payload only"),
    ("loom.hw_files", "#9466 hand-written diff file count; D1 payload only"),
    ("loom.generated_lines", "#9466 generated-path lines; D1 payload only"),
    ("loom.test_lines", "#9466 test-file lines; D1 payload only"),
];

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

/// The three keys #9586 admitted: each is exported by the daemon as a
/// `sweep.outcome` log attribute AND read by the sweep-facts extraction, so
/// the gateway stripping it was the exact silent-NULL this seam guards
/// against. Per-key (not table-driven) so a failure names the loss.
#[test]
fn admitted_sweep_outcome_fact_keys_survive_the_gateway_log_allowlist() {
    let keys = allowlist("log");
    for (key, why) in [
        (
            "loom.disposition",
            "#9441: the OTLP mapping (mapping.rs, SweepOutcome arm) exports it on EVERY \
             sweep.outcome record, and the extraction's landing predicate reads it — \
             stripping it made every gateway-side sweep read as not-landed",
        ),
        (
            "loom.tokens_unattributed_in",
            "#9443: metadata.rs exports the Σphases+remainder==total reconciliation input; \
             stripping it made the per-phase token check silently unanswerable",
        ),
        (
            "loom.tokens_unattributed_out",
            "#9443: metadata.rs exports the Σphases+remainder==total reconciliation input; \
             stripping it made the per-phase token check silently unanswerable",
        ),
    ] {
        assert!(keys.contains(key), "the gateway's log keep_keys strips `{key}` ({why})");
    }
}

/// The #9586 generalized guard: every `loom.*` key the sweep-facts
/// extraction reads must either survive the gateway's log `keep_keys` or be
/// pinned OTLP-absent above — a key in neither list is exactly the silent
/// NULL this file exists to make loud, so the next stripped key fails the
/// build instead of NULLing a column.
#[test]
fn every_sweep_facts_attribute_read_survives_the_gateway_or_is_pinned_absent() {
    let allowed = allowlist("log");
    let pinned: BTreeSet<&str> = OTLP_LOG_ABSENT.iter().map(|(key, _)| *key).collect();
    for (backend, sql) in [
        ("ClickStack", CLICKSTACK_EXTRACT),
        ("SigNoz", SIGNOZ_EXTRACT),
    ] {
        let reads = loom_attribute_reads(sql);
        assert!(
            !reads.is_empty(),
            "parsed no loom.* attribute reads out of the {backend} extraction view"
        );
        for key in &reads {
            assert!(
                allowed.contains(key) || pinned.contains(key.as_str()),
                "the {backend} extraction reads `{key}`, but the gateway's log keep_keys \
                 strips it and it is not on the pinned OTLP-absent list. Decide per key \
                 (#9586): if the daemon exports it as a sweep.outcome log attribute \
                 (observability/otlp/mapping.rs, mapping/metadata.rs), admit it in \
                 defaults/observability/collector/config.yaml; if it does not, pin it in \
                 OTLP_LOG_ABSENT with its evidence — never admit a key nothing sends"
            );
        }
    }
}

/// Key-level mirror of the column-level parity contract in
/// `sweep_facts_artifacts.rs`: the two backends' views must read the same
/// attribute keys, or one backend's facts silently stop being the other's.
#[test]
fn both_extractions_read_the_same_loom_attribute_keys() {
    assert_eq!(
        loom_attribute_reads(CLICKSTACK_EXTRACT),
        loom_attribute_reads(SIGNOZ_EXTRACT),
        "the ClickStack and SigNoz extraction views no longer read the same loom.* \
         attribute keys; a key read on one backend only is a fact the other backend \
         cannot produce (#9446 parity, #9586)"
    );
}

/// The pinned absence list is a decision, not a drawer: an entry whose key
/// the gateway now admits, or that no extraction reads, is stale and must go.
#[test]
fn the_pinned_absence_list_stays_honest() {
    let allowed = allowlist("log");
    let reads = loom_attribute_reads(CLICKSTACK_EXTRACT);
    assert_eq!(
        reads,
        loom_attribute_reads(SIGNOZ_EXTRACT),
        "parity precondition failed; see both_extractions_read_the_same_loom_attribute_keys"
    );
    for (key, why) in OTLP_LOG_ABSENT {
        assert!(
            !allowed.contains(*key),
            "`{key}` is pinned OTLP-absent but the gateway's log keep_keys now admits it; \
             if the daemon exports it as a sweep.outcome log attribute, delete the stale \
             pin so the survival rule owns the key — otherwise remove the dead allowlist \
             entry"
        );
        assert!(
            reads.contains(*key),
            "`{key}` is pinned OTLP-absent ({why}) but neither extraction view reads it; a \
             pin for an unread key is noise — delete the entry"
        );
    }
}

/// Regression guard for concurrent-edit clobbering of the allowlists. Each
/// `keep_keys` list is one very long line, so a stale-copy conflict
/// resolution silently drops keys other PRs added. That has happened more
/// than once (this PR's first revision deleted every key below). Pin one
/// representative key from each previously clobbered slice in the context
/// that carries it.
#[test]
fn previously_clobbered_allowlist_keys_survive_their_contexts() {
    let pinned: &[(&str, &str, &str)] = &[
        ("log", "loom.ci.dependency_wait_ms", "#9457 ci.job `needs:` wait segment"),
        ("span", "loom.ci.dependency_wait_ms", "#9457 ci.job span `needs:` wait segment"),
        ("span", "loom.host.pressure", "#9051 host-pressure slice"),
        ("span", "loom.admission.reason", "#9051 admission slice"),
    ];
    for (context, key, why) in pinned {
        assert!(
            allowlist(context).contains(*key),
            "the gateway's {context} keep_keys no longer admits `{key}` ({why}); likely a \
             stale-copy merge resolution of defaults/observability/collector/config.yaml. \
             Rebuild the edit from current main instead of dropping other PRs' keys"
        );
    }
}
