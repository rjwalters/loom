//! Tests for the record-kind registry (`telemetry/kinds.rs`, Issue #8921).
//!
//! Three properties, in order of how much they are worth:
//!
//! 1. **Two independent branches that each add a record kind merge cleanly.**
//!    That is the whole point of the issue — #8915 and #8909 were each clean
//!    and jointly unmergeable — so it is asserted against real `git`, not
//!    assumed from the `.gitattributes` line being present.
//! 2. **No pre-#8921 kind's wire contract moved.** Collapsing the
//!    `schema_version` ladder into a `gate:` column must be byte-identical for
//!    every kind that already shipped; the frozen table below is the guard, and
//!    it never needs appending to (post-#8921 kinds share
//!    [`NEW_KIND_SCHEMA_VERSION`], so nobody has to touch these rows again).
//! 3. **A union merge cannot pass silently broken.** Union merge takes both
//!    sides, which is right for independent rows and wrong for two edits to the
//!    same row; the uniqueness and coherence tests here are what turn that into
//!    a loud `cargo test` failure rather than a bad wire tag in production.

use super::*;
use crate::telemetry::kinds::{TelemetryKindOtlp, NEW_KIND_SCHEMA_VERSION, TELEMETRY_KINDS};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Path of the registry, relative to the repository root — the same string the
/// `.gitattributes` pattern uses.
const REGISTRY_PATH: &str = "loom-daemon/src/telemetry/kinds.rs";

/// The `.gitattributes` line that makes two concurrent row appends merge
/// instead of conflict.
const UNION_ATTRIBUTE: &str = "loom-daemon/src/telemetry/kinds.rs merge=union";

/// The marker a new row is appended above — load-bearing for the merge test,
/// and documented as such in the registry itself.
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

// ---------------------------------------------------------------------------
// 1. The property the issue is about: concurrent additions merge.
// ---------------------------------------------------------------------------

#[test]
fn gitattributes_marks_the_registry_as_union_merge() {
    let attributes = std::fs::read_to_string(repo_root().join(".gitattributes"))
        .expect("repository has a .gitattributes");
    assert!(
        attributes
            .lines()
            .any(|line| line.trim() == UNION_ATTRIBUTE),
        ".gitattributes must carry `{UNION_ATTRIBUTE}` — without it two concurrent \
         record-kind additions conflict again (#8921)"
    );
}

#[test]
fn the_registry_keeps_its_append_marker() {
    let registry =
        std::fs::read_to_string(repo_root().join(REGISTRY_PATH)).expect("the registry file exists");
    assert!(
        registry.contains(APPEND_MARKER),
        "{REGISTRY_PATH} must keep the `{APPEND_MARKER}` marker: it is where a new kind's \
         single row goes, and what the concurrent-merge test appends to"
    );
}

/// Simulates the #8915-vs-#8909 collision on the post-#8921 layout: two
/// branches, each appending one record-kind row at the same append point, with
/// no knowledge of each other. Under the `merge=union` attribute the merge must
/// succeed and keep **both** rows.
#[test]
fn two_branches_each_adding_a_record_kind_merge_cleanly() {
    let root = repo_root();
    let registry =
        std::fs::read_to_string(root.join(REGISTRY_PATH)).expect("the registry file exists");
    let attributes = std::fs::read_to_string(root.join(".gitattributes"))
        .expect("repository has a .gitattributes");

    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: no git on PATH");
        return;
    }

    let scratch = tempfile::tempdir().expect("tempdir");
    let repo = scratch.path();
    let run = |args: &[&str]| {
        let output = Command::new("git")
            .args(["-C", repo.to_str().expect("utf-8 tempdir")])
            .args([
                "-c",
                "user.email=loom@example.invalid",
                "-c",
                "user.name=Loom Test",
            ])
            .args(args)
            .output()
            .expect("git runs");
        (output.status.success(), String::from_utf8_lossy(&output.stderr).into_owned())
    };

    // A repository shaped like this one: same registry path, same attributes.
    std::fs::create_dir_all(repo.join(REGISTRY_PATH).parent().expect("has a parent"))
        .expect("mkdir -p");
    std::fs::write(repo.join(REGISTRY_PATH), &registry).expect("seed the registry");
    std::fs::write(repo.join(".gitattributes"), &attributes).expect("seed .gitattributes");
    assert!(run(&["init", "-q", "-b", "base"]).0, "git init");
    assert!(run(&["add", "-A"]).0, "git add");
    assert!(run(&["commit", "-q", "-m", "base"]).0, "git commit");

    // Each branch appends exactly one row, immediately above the marker —
    // i.e. both insert at the same line, the worst case for a textual merge.
    let row = |variant: &str, kind: &str| {
        format!(
            "            /// Synthetic kind for the concurrent-merge test.\n\
             \x20           {variant} = \"{kind}\" => $crate::telemetry::HostHealthRecord,\n\
             \x20               gate: $crate::telemetry::NEW_KIND_SCHEMA_VERSION, \
             otlp: Logs, native: true;\n\n"
        )
    };
    let with_row = |variant: &str, kind: &str| {
        let insertion = row(variant, kind);
        let at = registry.find(APPEND_MARKER).expect("marker present");
        // Insert at the start of the marker's own line.
        let line_start = registry[..at].rfind('\n').map_or(0, |i| i + 1);
        let mut patched = String::with_capacity(registry.len() + insertion.len());
        patched.push_str(&registry[..line_start]);
        patched.push_str(&insertion);
        patched.push_str(&registry[line_start..]);
        patched
    };

    for (branch, variant, kind) in [
        ("kind-alpha", "SyntheticAlpha", "synthetic.alpha"),
        ("kind-beta", "SyntheticBeta", "synthetic.beta"),
    ] {
        assert!(run(&["checkout", "-q", "base"]).0, "checkout base");
        assert!(run(&["checkout", "-q", "-b", branch]).0, "branch {branch}");
        std::fs::write(repo.join(REGISTRY_PATH), with_row(variant, kind))
            .expect("write the branch's row");
        assert!(run(&["commit", "-q", "-am", branch]).0, "commit {branch}");
    }

    assert!(run(&["checkout", "-q", "kind-alpha"]).0, "checkout kind-alpha");
    let (merged, stderr) = run(&["merge", "--no-edit", "-q", "kind-beta"]);
    assert!(
        merged,
        "two branches each adding one record-kind row must merge cleanly \
         (that is the whole point of #8921); git said: {stderr}"
    );

    let result = std::fs::read_to_string(repo.join(REGISTRY_PATH)).expect("read merged registry");
    assert!(!result.contains("<<<<<<<"), "merged registry must carry no conflict markers");
    for kind in ["synthetic.alpha", "synthetic.beta"] {
        assert!(result.contains(kind), "the union merge must keep `{kind}`'s row");
    }
    // And the pre-existing rows survive untouched.
    assert!(result.contains("\"queue.snapshot\""), "existing rows must survive the merge");
}

// ---------------------------------------------------------------------------
// 2. No pre-#8921 kind's wire contract moved.
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
#[test]
fn the_new_kind_gate_stays_above_the_shared_current_version() {
    assert!(
        CURRENT_SCHEMA_VERSION < NEW_KIND_SCHEMA_VERSION,
        "CURRENT_SCHEMA_VERSION ({CURRENT_SCHEMA_VERSION}) has reached \
         NEW_KIND_SCHEMA_VERSION ({NEW_KIND_SCHEMA_VERSION}); raise the latter so the \
         unpinned kinds and the post-#8921 kinds stay distinguishable on the wire"
    );
}

// ---------------------------------------------------------------------------
// 3. A union merge cannot pass silently broken.
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
                "duplicate variant `{}` — a union merge of two edits to the SAME row, not \
                 two independent rows",
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
