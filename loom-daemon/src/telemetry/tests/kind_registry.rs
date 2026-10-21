//! Tests for the record-kind registry (`telemetry/kinds.rs`, Issue #8921).
//!
//! Two properties:
//!
//! 1. **No pre-#8921 kind's wire contract moved.** Collapsing the
//!    `schema_version` ladder into a `gate:` column must be byte-identical for
//!    every kind that already shipped; the frozen table below is the guard, and
//!    it never needs appending to (post-#8921 kinds share
//!    [`NEW_KIND_SCHEMA_VERSION`], so nobody has to touch these rows again).
//! 2. **The registry stays coherent.** Wire tags and variants are unique and
//!    each row is well-formed, so two edits that both land in the table turn
//!    into a loud `cargo test` failure rather than a bad wire tag in production.

use super::*;
use crate::telemetry::kinds::{TelemetryKindOtlp, NEW_KIND_SCHEMA_VERSION, TELEMETRY_KINDS};
use std::path::{Path, PathBuf};

/// Path of the registry, relative to the repository root — the same string the
/// `.gitattributes` pattern uses.
const REGISTRY_PATH: &str = "loom-daemon/src/telemetry/kinds.rs";

/// The marker a new row is appended above, documented as such in the registry
/// itself.
const APPEND_MARKER: &str = "// APPEND NEW KINDS ABOVE THIS LINE";

/// Every record kind that existed when #8921 landed, with the exact envelope
/// `schema_version` it carried **before** the ladder moved out of
/// `envelope.rs`. Frozen wire contract: an entry may never change, and the list
/// never needs to grow (a new kind reports [`NEW_KIND_SCHEMA_VERSION`]).
///
/// `None` means "the kind never pinned its own gate and reports
/// [`CURRENT_SCHEMA_VERSION`]" — which is what the old `_ => ` fallback arm did,
/// including after a future bump of that constant.
const FROZEN_GATES: &[(&str, Option<u32>)] = &[
    ("sweep.started", None),
    ("sweep.identity", Some(4)),
    ("sweep.phase", None),
    ("sweep.completed", None),
    ("sweep.outcome", None),
    ("tokens.snapshot", None),
    ("host.health", None),
    ("role_tick.outcome", None),
    ("session.summary", Some(5)),
    ("session.analysis", Some(6)),
    ("daemon.event", Some(7)),
    ("trace.span", Some(3)),
    ("ci.run", Some(8)),
    ("ci.job", Some(8)),
    ("ci.duration", Some(8)),
    ("ci.job.log", Some(9)),
    ("metric.points", Some(10)),
    ("queue.snapshot", Some(11)),
];

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<root>/loom-daemon`, in a worktree as well as in
    // the primary clone.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon has a parent directory")
        .to_path_buf()
}

#[test]
fn the_registry_keeps_its_append_marker() {
    let registry =
        std::fs::read_to_string(repo_root().join(REGISTRY_PATH)).expect("the registry file exists");
    assert!(
        registry.contains(APPEND_MARKER),
        "{REGISTRY_PATH} must keep the `{APPEND_MARKER}` marker: it is where a new kind's \
         single row goes"
    );
}

// ---------------------------------------------------------------------------
// 1. No pre-#8921 kind's wire contract moved.
// ---------------------------------------------------------------------------

#[test]
fn every_pre_8921_kind_keeps_the_exact_gate_version_it_shipped_with() {
    for (kind, expected) in FROZEN_GATES {
        let row = TELEMETRY_KINDS
            .iter()
            .find(|meta| meta.kind == *kind)
            .unwrap_or_else(|| panic!("kind `{kind}` must stay registered"));
        let expected = expected.unwrap_or(CURRENT_SCHEMA_VERSION);
        assert_eq!(
            row.schema_version, expected,
            "kind `{kind}` shipped envelopes at schema_version {expected}; changing it \
             silently breaks a mixed-version fleet's backend gate"
        );
    }
}

#[test]
fn the_new_kind_gate_is_above_every_frozen_gate() {
    for (kind, pinned) in FROZEN_GATES {
        if let Some(pinned) = pinned {
            assert!(
                *pinned < NEW_KIND_SCHEMA_VERSION,
                "NEW_KIND_SCHEMA_VERSION ({NEW_KIND_SCHEMA_VERSION}) must be above every \
                 hand-allocated gate; `{kind}` pins {pinned}"
            );
        }
    }
}

/// `CURRENT_SCHEMA_VERSION` is a *separate* sequence from the per-kind gates:
/// it is bumped on a breaking change to the shared record shapes, and the kinds
/// that never pinned a gate follow it. If a future bump ever reached
/// [`NEW_KIND_SCHEMA_VERSION`], those kinds and every post-#8921 kind would
/// report the same number and the version would stop distinguishing them —
/// which is the one way the fixed `12` can go wrong. Raise
/// `NEW_KIND_SCHEMA_VERSION` (and add a `Version history` row in
/// `defaults/docs/telemetry-schema.md`) in the same change that bumps
/// `CURRENT_SCHEMA_VERSION` that far.
///
/// Both operands are compile-time constants, so this is a `const` **item** and
/// not a `#[test]`: the offending bump then fails to *compile*, which is earlier
/// than a test failure and cannot be skipped by a filtered test run. (A runtime
/// `assert!` on two `const`s is also what `clippy::assertions_on_constants`
/// rejects under the lint job's `--all-targets -D warnings`.)
///
/// It has to be a `const _` item rather than an inline `const { … }` block
/// inside a test fn: an inline block in a never-called `#[test]` body is *not*
/// evaluated by `cargo check`/`cargo clippy --all-targets`, so that spelling
/// would compile happily with `CURRENT_SCHEMA_VERSION = 12`. This one does not —
/// verified by bumping the constant locally. `const` evaluation has no
/// `format_args!`, hence the static message.
const _: () = assert!(
    CURRENT_SCHEMA_VERSION < NEW_KIND_SCHEMA_VERSION,
    "CURRENT_SCHEMA_VERSION has reached NEW_KIND_SCHEMA_VERSION (12); raise the latter so \
     the unpinned kinds and the post-#8921 kinds stay distinguishable on the wire — see \
     NEW_KIND_SCHEMA_VERSION in telemetry/kinds.rs"
);

// ---------------------------------------------------------------------------
// 2. The registry stays coherent.
// ---------------------------------------------------------------------------

#[test]
fn wire_tags_and_variant_names_are_unique() {
    for (index, meta) in TELEMETRY_KINDS.iter().enumerate() {
        for other in &TELEMETRY_KINDS[index + 1..] {
            assert_ne!(
                meta.kind, other.kind,
                "duplicate wire tag `{}` ({} and {}) — two rows claim the same `kind`",
                meta.kind, meta.variant, other.variant
            );
            assert_ne!(
                meta.variant, other.variant,
                "duplicate variant `{}` — two edits to the SAME row, not two \
                 independent rows",
                meta.variant
            );
        }
    }
}

#[test]
fn every_kind_reaches_at_least_one_backend() {
    for meta in TELEMETRY_KINDS {
        assert!(
            meta.native_ingest || meta.otlp != TelemetryKindOtlp::NotExported,
            "kind `{}` declares `native: false` and `otlp: NotExported`, so nothing would \
             ever carry it anywhere",
            meta.kind
        );
    }
}

#[test]
fn the_registry_row_agrees_with_the_record_it_describes() {
    for record in every_record() {
        let meta = TELEMETRY_KINDS
            .iter()
            .find(|meta| meta.kind == record.kind())
            .unwrap_or_else(|| panic!("`{}` must have a registry row", record.kind()));

        // The registry's `kind` is the serde tag, not a parallel spelling.
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(
            value.get("kind").and_then(serde_json::Value::as_str),
            Some(meta.kind),
            "registry tag and serde tag must be the same string"
        );
        assert_eq!(meta.schema_version, record.schema_version());
        assert_eq!(meta.otlp, record.otlp_class());
        assert_eq!(meta.native_ingest, record.accepted_by_native_ingest());
        assert_eq!(
            TelemetryEnvelope::new("host-abc", record.clone()).schema_version,
            meta.schema_version,
            "the envelope must stamp the kind's declared gate"
        );
    }
}

/// The Stage 1 exported-facts contract (`defaults/docs/telemetry-schema.md`,
/// #11098): every record external consumers rely on stays registered with its
/// signal shape, and the retired ETA-only wire tags stay retired (never
/// reused) after the ETA subsystem was deleted.
#[test]
fn exported_facts_contract_kinds_are_registered_and_retired_tags_stay_gone() {
    let kind = |tag: &str| TELEMETRY_KINDS.iter().find(|k| k.kind == tag);
    // (tag, exports over OTLP, accepted by the native HTTPS backend)
    for (tag, otlp, native) in [
        ("sweep.outcome", true, true),
        ("sweep.started", true, true),
        ("sweep.phase", true, true),
        ("sweep.completed", true, true),
        ("sweep.identity", true, true),
        ("pick.decision", true, false),
        ("queue.snapshot", false, true),
        ("fleet.state", true, false),
        ("host.health", true, true),
        ("host.export", true, false),
        ("operator.presence", true, false),
        ("pr.resolved", true, false),
        ("eta.stage_outcome", true, false),
    ] {
        let meta = kind(tag).unwrap_or_else(|| panic!("contract kind `{tag}` is not registered"));
        assert_eq!(
            meta.otlp != TelemetryKindOtlp::NotExported,
            otlp,
            "`{tag}`: OTLP export changed"
        );
        assert_eq!(meta.native_ingest, native, "`{tag}`: native ingest changed");
    }
    for tag in [
        "eta.estimate",
        "eta.outcome",
        "eta.snapshot",
        "eta.fit",
        "eta.fleet_refresh",
        "eta.backtest.fold",
        "eta.backtest.summary",
    ] {
        assert!(kind(tag).is_none(), "retired wire tag `{tag}` was re-registered");
    }
}
