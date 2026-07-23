//! Doc-lint test for `defaults/.claude/commands/loom/bump.md` (Issue #3468).
//!
//! The `/loom:bump` skill is the generic, consumer-facing quick-bump. The full
//! release methodology now lives in `/repo:release` (rjwalters/repo, #3563);
//! `/loom:release` was retired. `/loom:bump` must ship to consumer repos (it is
//! NOT in `defaults/.loom-internal.list`) and its prose must document a
//! specific contract: seven detection sources, eight lifecycle phases, an
//! explicit-confirmation gate on push + GitHub Release, and a parameterized
//! `scripts/version.sh` template that subsequent runs reuse.
//!
//! This test grep-checks the markdown file at compile time so that:
//!
//! - Renames/refactors to the phase headings flag a CI failure.
//! - Removing a detection source by accident also flags a CI failure.
//! - The acceptance criteria for #3468 (AC #1 through AC #8) can be
//!   verified programmatically.
//!
//! Companion tests: `sweep_md_doc_lint.rs` (Phase B, #3453),
//! `sweep_md_stage_minus_one_doc_lint.rs` (Phase D, #3454).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;

const BUMP_MD_RELATIVE: &str = "../defaults/.claude/commands/loom/bump.md";

fn read_bump_md() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(BUMP_MD_RELATIVE);
    fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "bump.md not found at {} (CWD-relative path: {}): {e}",
            path.display(),
            BUMP_MD_RELATIVE,
        );
    })
}

/// AC: skill file exists with the expected title.
#[test]
fn bump_md_exists_and_has_title() {
    let content = read_bump_md();
    assert!(
        content.starts_with("# Version Bump + Tag"),
        "expected `# Version Bump + Tag` title at top of bump.md — \
         the skill must self-identify as the generic version-bump skill"
    );
}

/// AC: the eight lifecycle phases must all be present as section headers.
///
/// Phase 1: Detect version sources
/// Phase 2: Ensure CHANGELOG.md exists
/// Phase 3: Compute the new version
/// Phase 4: Draft the changelog entry
/// Phase 5: Generate (or update) scripts/version.sh
/// Phase 6: Run the bump + tag flow
/// Phase 7: Push and gh release create (OPTIONAL)
/// Phase 8: Summary
#[test]
fn bump_md_documents_all_eight_phases() {
    let content = read_bump_md();
    // Assert PRESENCE of all eight lifecycle phases by number, not by exact
    // title. Phase titles are prose that legitimately evolves (e.g. #3856 made
    // the CHANGELOG phase optional, renaming "Ensure…" to "Update…if present"),
    // so pinning the full title makes this doc-lint brittle and red-mains main
    // on legitimate edits (#3860/#3861). The contract is "eight numbered phases
    // exist as section headers"; that is what we check.
    let required_phase_headers: &[&str] = &[
        "## Phase 1:",
        "## Phase 2:",
        "## Phase 3:",
        "## Phase 4:",
        "## Phase 5:",
        "## Phase 6:",
        "## Phase 7:",
        "## Phase 8:",
    ];
    for header in required_phase_headers {
        assert!(
            content.contains(header),
            "bump.md is missing required phase header `{header}` — \
             #3468 acceptance criteria require eight lifecycle phases"
        );
    }
}

/// AC3, AC4, AC5: detection prose must mention all seven version-source
/// shapes. The skill prose tells the runtime LLM which files to scan; if
/// any source disappears from this list the contract is broken.
#[test]
fn bump_md_lists_seven_detection_sources() {
    let content = read_bump_md();
    // AC3: multi-file npm+cargo monorepo shape (Loom-style).
    assert!(
        content.contains("package.json"),
        "bump.md must document `package.json` detection (AC3, npm shape)"
    );
    assert!(
        content.contains("*/package.json"),
        "bump.md must document `*/package.json` workspace-package detection \
         (AC3, npm-workspace shape — used by Loom for mcp-loom/)"
    );
    assert!(
        content.contains("Cargo.toml"),
        "bump.md must document `Cargo.toml` detection (AC3, cargo shape)"
    );
    assert!(
        content.contains("Cargo.lock"),
        "bump.md must document `Cargo.lock` refresh requirement (AC3, cargo-workspace shape)"
    );
    // AC5: pyproject.toml shape.
    assert!(
        content.contains("pyproject.toml"),
        "bump.md must document `pyproject.toml` detection (AC5, Python shape)"
    );
    assert!(
        content.contains("[project].version") || content.contains("`[project].version`"),
        "bump.md must document `[project].version` PEP-621 detection (AC5)"
    );
    assert!(
        content.contains("[tool.poetry].version") || content.contains("`[tool.poetry].version`"),
        "bump.md must document `[tool.poetry].version` Poetry detection (AC5)"
    );
    // AC4: rjwalters/clean shape — top-level shell script with VERSION="X.Y.Z".
    assert!(
        content.contains("VERSION=\"X.Y.Z\"") || content.contains(r#"VERSION="X.Y.Z""#),
        "bump.md must document `VERSION=\"X.Y.Z\"` shell-script detection (AC4, rjwalters/clean shape)"
    );
    // Markdown version shape (CLAUDE.md / README.md).
    assert!(
        content.contains("**Version**: X.Y.Z") || content.contains("`**Version**: X.Y.Z`"),
        "bump.md must document `**Version**: X.Y.Z` markdown detection (Loom CLAUDE.md shape)"
    );
    assert!(
        content.contains("CLAUDE.md") && content.contains("README.md"),
        "bump.md must reference both `CLAUDE.md` and `README.md` as scan targets"
    );
}

/// AC6: the skill must ship a templated `scripts/version.sh` body. We
/// assert the marker structural pieces of the template (function names,
/// subcommand wiring) so a future refactor that drops the template by
/// accident lights up CI.
#[test]
fn bump_md_includes_version_sh_template() {
    let content = read_bump_md();
    let required_template_markers: &[&str] = &[
        "scripts/version.sh",
        "VERSION_FILES=",
        "get_version()",
        "get_version_from_file()",
        "check_versions()",
        "bump_version()",
        "set_version()",
        "do_tag()",
        // The subcommand surface mirrored from Loom's own scripts/version.sh.
        "bump <major|minor|patch>",
        "set <version>",
        "--tag",
    ];
    for marker in required_template_markers {
        assert!(
            content.contains(marker),
            "bump.md is missing template marker `{marker}` — \
             #3468 AC6 requires a parameterized scripts/version.sh template"
        );
    }
}

/// AC7: CHANGELOG handling must ensure `## [Unreleased]` and offer to
/// scaffold a Keep-a-Changelog header when CHANGELOG.md is absent.
#[test]
fn bump_md_documents_changelog_handling() {
    let content = read_bump_md();
    assert!(
        content.contains("## [Unreleased]"),
        "bump.md must reference the `## [Unreleased]` Keep-a-Changelog heading (AC7)"
    );
    assert!(
        content.contains("Keep a Changelog") || content.contains("keepachangelog"),
        "bump.md must reference Keep-a-Changelog convention (AC7)"
    );
    // The promotion transform: [Unreleased] -> [X.Y.Z] - YYYY-MM-DD.
    assert!(
        content.contains("YYYY-MM-DD"),
        "bump.md must describe the `[Unreleased] -> [X.Y.Z] - YYYY-MM-DD` promotion (AC7)"
    );
}

/// AC8: explicit-confirmation gate on push + GitHub Release.
#[test]
fn bump_md_gates_push_and_release_on_confirmation() {
    let content = read_bump_md();
    // The Phase 7 header must call itself OPTIONAL.
    assert!(
        content.contains("Phase 7") && content.contains("OPTIONAL"),
        "bump.md Phase 7 must be marked OPTIONAL — #3468 AC8 requires an \
         explicit confirmation gate before push + gh release create"
    );
    // The skill must invoke `gh release create` (or describe doing so).
    assert!(
        content.contains("gh release create"),
        "bump.md must document `gh release create` as the GitHub Release step (AC8)"
    );
    // The skill must NOT publish to package registries — load-bearing
    // safety guardrail per the issue's "out of scope" list.
    assert!(
        content.contains("npm publish") || content.contains("not run `npm publish`"),
        "bump.md must explicitly disclaim registry publication (npm publish, cargo publish, twine upload)"
    );
}

/// Acceptance check that the skill self-identifies as the generic quick-bump
/// and points at `/repo:release` for the full release methodology (so readers
/// understand when to reach for the lightweight bump vs. the full flow). The
/// retired `/loom:release` skill (#3563) must NOT be referenced.
#[test]
fn bump_md_distinguishes_itself_from_repo_release() {
    let content = read_bump_md();
    assert!(
        content.contains("/repo:release"),
        "bump.md must reference `/repo:release` so readers know where the full \
         release methodology lives (rjwalters/repo, #3563)"
    );
    assert!(
        content.contains("generic"),
        "bump.md must describe itself as the generic quick-bump"
    );
    assert!(
        !content.contains("/loom:release"),
        "bump.md must NOT reference the retired `/loom:release` skill (#3563)"
    );
}

/// AC2 by transitive contract: the skill file must NOT be listed in
/// `defaults/.loom-internal.list` (that would prevent it from shipping
/// to consumers — the opposite of AC1).
#[test]
fn bump_md_is_not_in_loom_internal_skip_list() {
    let skip_list_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../defaults/.loom-internal.list");
    let skip_list = fs::read_to_string(&skip_list_path).unwrap_or_else(|e| {
        panic!("defaults/.loom-internal.list not found at {}: {e}", skip_list_path.display());
    });
    for line in skip_list.lines() {
        // Strip comments and trim.
        let entry = match line.split_once('#') {
            Some((before, _)) => before.trim(),
            None => line.trim(),
        };
        if entry.is_empty() {
            continue;
        }
        assert_ne!(
            entry, ".claude/commands/loom/bump.md",
            "bump.md must NOT be on defaults/.loom-internal.list — it is the \
             generic skill that ships to consumers. #3468 AC1 requires this."
        );
    }
}
