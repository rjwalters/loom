//! Doc-lint test for Phase E of epic #3449 (issue #3456).
//!
//! Phase E deprecates `defaults/scripts/spawn-loop.sh` and rewrites the
//! operator-facing documentation to describe the actual Phase A-D
//! `loom-daemon` MCP surface instead of the v0.9.x stop-gap warnings.
//!
//! This test grep-checks the operator-facing markdown files at compile
//! time so that:
//!
//! - The legacy strings `LOOM_USE_SPAWN_LOOP` and `spawn-loop.sh start`
//!   do not appear in operator-facing docs outside the migration guide.
//! - The deprecation warning string is present in `spawn-loop.sh`.
//! - The migration narrative actually mentions the Phase A-D MCP surface
//!   (`mcp__loom__dispatch_sweep`, `mcp__loom__list_sweeps`).
//!
//! The migration guide (`docs/migration/v0.10.0-shepherd-deprecation.md`)
//! is **exempt** from the legacy-string ban — it is the historical
//! record and must be allowed to discuss the deprecated surface during
//! the v0.10.x window.
//!
//! Companion tests:
//! - `sweep_md_doc_lint.rs` (Phase B, #3453)
//! - `sweep_md_stage_minus_one_doc_lint.rs` (Phase D, #3454)

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;

/// Operator-facing files where the legacy spawn-loop strings must NOT
/// appear. The migration guide is intentionally absent from this list.
const BANNED_LEGACY_FILES: &[&str] = &[
    "../CLAUDE.md",
    "../defaults/CLAUDE.md",
    "../defaults/.claude/commands/loom/loom.md",
    "../defaults/.claude/commands/loom/sweep.md",
];

/// Strings that must not appear in any of the operator-facing files
/// above. They MAY appear in the migration guide (out-of-list).
const BANNED_LEGACY_STRINGS: &[&str] = &["LOOM_USE_SPAWN_LOOP", "spawn-loop.sh start"];

/// Files that MUST contain the deprecation warning machinery.
const DEPRECATION_WARNING_FILE: &str = "../defaults/scripts/spawn-loop.sh";

/// Strings that must be present in the deprecation warning.
const REQUIRED_DEPRECATION_STRINGS: &[&str] = &[
    "_deprecation_warn",
    "DEPRECATION WARNING",
    "#3449",
    "v0.11.0",
    "mcp__loom__dispatch_sweep",
    "LOOM_SUPPRESS_DEPRECATION",
];

/// Migration guide must mention the Phase A-D MCP surface so downstream
/// readers can find the replacement.
const MIGRATION_GUIDE_FILE: &str = "../docs/migration/v0.10.0-shepherd-deprecation.md";

const REQUIRED_MIGRATION_STRINGS: &[&str] = &[
    "mcp__loom__dispatch_sweep",
    "mcp__loom__list_sweeps",
    "loom-daemon",
];

fn read_file(rel_path: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel_path);
    fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("file not found at {} (CWD-relative path: {}): {e}", path.display(), rel_path,);
    })
}

/// AC #3: operator-facing CLAUDE.md must not reference `LOOM_USE_SPAWN_LOOP`
/// or `spawn-loop.sh start` as the recommended path.
///
/// Banned strings may appear in the migration guide — see the per-file
/// loop below for the explicit allowlist.
#[test]
fn operator_docs_do_not_reference_legacy_spawn_loop_strings() {
    let mut failures: Vec<String> = Vec::new();

    for file in BANNED_LEGACY_FILES {
        let content = read_file(file);
        for banned in BANNED_LEGACY_STRINGS {
            if content.contains(banned) {
                failures.push(format!(
                    "  - `{banned}` found in {file}; this string is banned in \
                     operator-facing docs (allowed only in the migration guide)"
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "Phase E of #3449 (issue #3456) requires that operator-facing docs \
         do not reference the deprecated spawn-loop surface. Move the \
         offending references to the migration guide \
         (docs/migration/v0.10.0-shepherd-deprecation.md), or replace them \
         with `mcp__loom__dispatch_sweep` against `loom-daemon`.\n\nFailures:\n{}",
        failures.join("\n"),
    );
}

/// AC #1 / #2: `defaults/scripts/spawn-loop.sh` must contain the deprecation
/// warning machinery referencing #3449, v0.11.0, and the
/// `mcp__loom__dispatch_sweep` migration hint, with `LOOM_SUPPRESS_DEPRECATION`
/// as the documented suppression env var.
#[test]
fn spawn_loop_sh_contains_deprecation_warning_machinery() {
    let content = read_file(DEPRECATION_WARNING_FILE);
    let mut missing: Vec<&str> = Vec::new();

    for required in REQUIRED_DEPRECATION_STRINGS {
        if !content.contains(required) {
            missing.push(required);
        }
    }

    assert!(
        missing.is_empty(),
        "defaults/scripts/spawn-loop.sh is missing the following required \
         deprecation-warning strings (Phase E of #3449):\n  {}\n\n\
         The warning must reference epic #3449, name the v0.11.0 removal \
         target, point at `mcp__loom__dispatch_sweep` as the replacement, \
         and document `LOOM_SUPPRESS_DEPRECATION=1` as the suppression \
         env var.",
        missing.join("\n  "),
    );
}

/// AC #4: the migration guide must reference the Phase A-D MCP surface
/// (`mcp__loom__dispatch_sweep`, `mcp__loom__list_sweeps`, `loom-daemon`)
/// so downstream consumers can find the replacement for the deprecated
/// spawn loop.
#[test]
fn migration_guide_references_phase_a_through_d_surface() {
    let content = read_file(MIGRATION_GUIDE_FILE);
    let mut missing: Vec<&str> = Vec::new();

    for required in REQUIRED_MIGRATION_STRINGS {
        if !content.contains(required) {
            missing.push(required);
        }
    }

    assert!(
        missing.is_empty(),
        "docs/migration/v0.10.0-shepherd-deprecation.md is missing the \
         following required references to the Phase A-D MCP surface:\n  {}\n\n\
         Phase E of #3449 requires the migration narrative point downstream \
         readers at the actual replacement (the Rust `loom-daemon` binary \
         and its MCP-tool surface).",
        missing.join("\n  "),
    );
}

/// Sanity check: the migration guide IS allowed to mention
/// `LOOM_USE_SPAWN_LOOP` and `spawn-loop.sh start` — verify it actually
/// does, so the "the migration guide is exempt" carve-out is doing real
/// work (and someone can't silently drop the legacy references from the
/// guide without us noticing).
#[test]
fn migration_guide_retains_legacy_references_for_continuity() {
    let content = read_file(MIGRATION_GUIDE_FILE);

    // The migration guide is allowed to reference the deprecated surface;
    // we expect at least one banned-elsewhere string to appear here so
    // the carve-out has teeth.
    let mentions_legacy = BANNED_LEGACY_STRINGS
        .iter()
        .any(|banned| content.contains(banned));

    assert!(
        mentions_legacy,
        "Expected the migration guide \
         (docs/migration/v0.10.0-shepherd-deprecation.md) to retain at \
         least one reference to the deprecated spawn-loop surface \
         (e.g., `LOOM_USE_SPAWN_LOOP` or `spawn-loop.sh start`) for \
         historical continuity. If you intentionally removed all such \
         references, also remove the carve-out from the \
         `operator_docs_do_not_reference_legacy_spawn_loop_strings` test \
         above so the doc-lint stays consistent.",
    );
}
