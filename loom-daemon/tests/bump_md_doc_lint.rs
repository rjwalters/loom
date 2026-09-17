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
//!
//! ---------------------------------------------------------------------------
//! Assertion classification (#3877 — prose vs. contract)
//! ---------------------------------------------------------------------------
//! Every `contains()` assertion below is tagged PROSE or CONTRACT:
//!
//! - **PROSE** — a section title / self-description / wording that legitimately
//!   gets edited. These assert STRUCTURE/PRESENCE (the H1 prefix, the phase
//!   NUMBER `## Phase N:`, a single stable self-identifying word) rather than
//!   exact wording, so a rename that keeps the section does NOT red-main `main`
//!   — only a deletion does. Precedent: #3856 renamed "Phase 2: Ensure…" →
//!   "Phase 2: Update…(optional)" → red main → #3863 patch switched to
//!   `## Phase N:` presence-by-number.
//! - **CONTRACT** — a stable identifier that MUST NOT drift: the seven
//!   detection-source shapes (filenames + version-string tokens the runtime
//!   LLM scans for), the `scripts/version.sh` template function/subcommand
//!   markers, the Keep-a-Changelog headings + date format, `gh release create`,
//!   the `npm publish` safety disclaimer, `/repo:release`, the retired
//!   `/loom:release` negative, and the `.loom-internal.list` path. These stay
//!   EXACT — their exactness is their value.
//!
//! ---------------------------------------------------------------------------
//! Before adding a NEW `contains()`/`find()` assertion (#7992)
//! ---------------------------------------------------------------------------
//! Read "Markdown Doc-Lint Tests: No New Prose-Existence Assertions" in
//! `tests/README.md` first. Short version: a literal that lives inside a code
//! fence should be extracted and EXECUTED (see the
//! `defaults/scripts/tests/test-guide-*.sh` pattern), not pinned as a string;
//! genuine prose with no executable surface should rely on review, not a new
//! `contains()` pin. The CONTRACT/PROSE split above (#3877) predates that rule
//! and stays as-is here — see #7979 for the tracked migration — but it is not
//! license to add a fresh sentence pin under a PROSE label.
//!
//! ---------------------------------------------------------------------------
//! #7996 migration status (split from #7979)
//! ---------------------------------------------------------------------------
//! Of this file's `contains()`/`starts_with()` needles, only the ten in the
//! old `bump_md_includes_version_sh_template` (`VERSION_FILES=`,
//! `get_version()`, `get_version_from_file()`, `check_versions()`,
//! `bump_version()`, `set_version()`, `do_tag()`,
//! `bump <major|minor|patch>`, `set <version>`, `--tag`) lived EXCLUSIVELY
//! inside a markdown code fence (verified by a fence-position scan of the
//! whole doc). That test is now `extract_fenced_block_after` + a real `bash
//! -n` syntax check + end-to-end execution (`show`/`check`/`bump
//! patch`/`set <version> --tag` against a real git fixture) of the fenced
//! `scripts/version.sh` template — see its doc comment below. There are no
//! table/structure assertions in this file (bump.md has no markdown tables).
//! Every remaining needle in this file also has at least one occurrence
//! OUTSIDE a code fence (in the surrounding prose itself), so per the rule
//! above it is not a fence-literal pin requiring migration — these stay
//! review-only, unchanged by #7996: `bump_md_documents_all_eight_phases`,
//! `bump_md_lists_seven_detection_sources`,
//! `bump_md_documents_changelog_handling`,
//! `bump_md_gates_push_and_release_on_confirmation`,
//! `bump_md_distinguishes_itself_from_repo_release`,
//! `bump_md_is_not_in_loom_internal_skip_list`, and
//! `bump_md_exists_and_has_title`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

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

/// Extracts the body of the first fenced code block (```` ```lang\n...\n``` ````)
/// that appears at or after the given `anchor` substring in `content`.
///
/// This is the extract-and-execute primitive (#7979/#7993 precedent,
/// `doc_lint_support::first_fenced_block_after`): rather than pinning the
/// fence's literal contents as a `contains()` string, the caller extracts the
/// fence body and RUNS it, so a behavior-preserving reword/relocation of the
/// surrounding prose (or the fence's own comments) does not break the build —
/// only an actual functional change does. Panics naming `anchor` if no fence
/// is found or it is never closed — exactly the "someone deleted/broke the
/// example" case this lint exists to catch.
fn extract_fenced_block_after<'a>(content: &'a str, anchor: &str) -> &'a str {
    let from = content
        .find(anchor)
        .unwrap_or_else(|| panic!("expected to find anchor `{anchor}` in bump.md"));
    let rest = &content[from..];
    let open = rest
        .find("```")
        .unwrap_or_else(|| panic!("expected a fenced code block after `{anchor}`, found none"));
    let after_open = &rest[open + 3..];
    let nl = after_open.find('\n').unwrap_or_else(|| {
        panic!("malformed fence after `{anchor}`: no newline following the opening ```")
    });
    let body = &after_open[nl + 1..];
    let close = body.find("```").unwrap_or_else(|| {
        panic!("unterminated fence after `{anchor}` — the code block never closes")
    });
    &body[..close]
}

/// AC: skill file exists with the expected title.
#[test]
fn bump_md_exists_and_has_title() {
    let content = read_bump_md();
    // PROSE (structural prefix): the H1 title text can gain a parenthetical
    // suffix (it is currently `# Version Bump + Tag (generic)`), so we assert
    // the stable `# Version Bump + Tag` PREFIX at the top of the file, not the
    // full title. Fails if the H1 is removed or renamed away from the skill.
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
    // PROSE (structural by number): assert PRESENCE of all eight lifecycle
    // phases by number, not by exact title. Phase titles are prose that
    // legitimately evolves (e.g. #3856 made
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
    // CONTRACT (all assertions in this test): each string is a filename or a
    // version-string token the runtime LLM greps for during detection. If any
    // disappears the detection contract is broken — these are not editorial
    // prose. Keep EXACT.
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

/// AC6: the skill must ship a templated `scripts/version.sh` body.
///
/// Migrated from a `contains()` pin over `VERSION_FILES=`, `get_version()`,
/// `get_version_from_file()`, `check_versions()`, `bump_version()`,
/// `set_version()`, `do_tag()`, `bump <major|minor|patch>`, `set <version>`,
/// and `--tag` (#7996 — split from #7979): every one of those markers lives
/// EXCLUSIVELY inside the fenced `scripts/version.sh` template (verified by a
/// fence-position scan of the whole doc at authoring time), so a correct
/// rename/reword inside the fence would previously break the build with a
/// message about "AC6 requires a template" instead of "two strings no longer
/// match" — the exact #7950/#7948 failure shape.
///
/// This test now EXTRACTS the fenced template verbatim, `bash -n` syntax-
/// checks the whole thing (including its commented-out per-shape
/// placeholders), then fills in the npm shape using the template's OWN
/// commented-out npm-shape example lines (what a runtime LLM following this
/// skill would uncomment for a real npm project) and RUNS the generated
/// script end-to-end — `show`, `check`, `bump patch`, and `set <version>
/// --tag` — against a real git fixture. It fails on an actual behavior
/// break (a function renamed, a subcommand's argument grammar changed, the
/// git add/commit/tag sequence broken), not on a wording change.
#[test]
fn bump_md_includes_version_sh_template() {
    let content = read_bump_md();

    // PROSE (structural anchors, not fenced-literal pins — both also appear
    // in the surrounding prose, e.g. Phase 5's heading and Phase 1 item 1):
    // kept as simple presence checks so the extraction anchor below has a
    // named failure mode if the heading disappears.
    assert!(
        content.contains("scripts/version.sh"),
        "bump.md must reference `scripts/version.sh` — #3468 AC6"
    );
    let anchor = "#### Template (adapt to detected sources)";
    assert!(
        content.contains(anchor),
        "bump.md must retain the '{anchor}' heading anchoring the extracted \
         scripts/version.sh template"
    );

    let fence = extract_fenced_block_after(&content, anchor);

    // Syntax check: the WHOLE template (including its commented-out
    // per-shape placeholder sections) must parse as valid bash.
    let syntax = Command::new("bash")
        .arg("-n")
        .arg("-c")
        .arg(fence)
        .output()
        .expect("run `bash -n` on the extracted scripts/version.sh template");
    assert!(
        syntax.status.success(),
        "the scripts/version.sh template fenced in bump.md is not valid bash: {}",
        String::from_utf8_lossy(&syntax.stderr)
    );

    // Fill in the npm shape using the template's own commented-out npm-shape
    // example lines. If either of these substrings disappears (the template
    // dropped its npm-shape example), fail with a clear message rather than
    // silently running an unfilled (non-functional) template.
    let version_files_placeholder = "  # \"package.json\"            # npm shape";
    assert!(
        fence.contains(version_files_placeholder),
        "scripts/version.sh template must retain its commented-out npm-shape \
         VERSION_FILES example (`{version_files_placeholder}`) — this test \
         fills it in to exercise the template end-to-end"
    );
    let get_version_placeholder = "  # jq -r '.version' \"$REPO_ROOT/package.json\"\n  echo \"REPLACE_WITH_CANONICAL_SOURCE_READER\"";
    assert!(
        fence.contains(get_version_placeholder),
        "scripts/version.sh template must retain its commented-out npm-shape \
         get_version() example — this test fills it in to exercise the \
         template end-to-end"
    );
    // The `set_version()` per-shape writer for JSON files (the actual code
    // that mutates package.json). Same treatment: uncomment the template's
    // own npm-shape example, restricted to the single fixture file.
    let json_writer_placeholder = "  # JSON files (npm / npm-workspace):\n  \
         # for file in package.json mcp-loom/package.json; do\n  \
         #   local tmp; tmp=$(mktemp)\n  \
         #   jq --arg v \"$new_version\" '.version = $v' \"$REPO_ROOT/$file\" > \"$tmp\"\n  \
         #   mv \"$tmp\" \"$REPO_ROOT/$file\"\n  \
         #   echo \"  Updated $file\"\n  \
         # done";
    assert!(
        fence.contains(json_writer_placeholder),
        "scripts/version.sh template must retain its commented-out npm-shape \
         JSON writer example in set_version() — this test fills it in to \
         exercise the template end-to-end"
    );
    let json_writer_filled = "  # JSON files (npm / npm-workspace):\n  \
         for file in package.json; do\n    \
         local tmp; tmp=$(mktemp)\n    \
         jq --arg v \"$new_version\" '.version = $v' \"$REPO_ROOT/$file\" > \"$tmp\"\n    \
         mv \"$tmp\" \"$REPO_ROOT/$file\"\n    \
         echo \"  Updated $file\"\n  \
         done";
    let filled = fence
        .replace(version_files_placeholder, "  \"package.json\"")
        .replace(get_version_placeholder, "  jq -r '.version' \"$REPO_ROOT/package.json\"")
        .replace(json_writer_placeholder, json_writer_filled);

    // Write the filled-in script into a real git fixture and run it.
    let sandbox = tempfile::tempdir().expect("create sandbox dir");
    let scripts_dir = sandbox.path().join("scripts");
    fs::create_dir_all(&scripts_dir).expect("create scripts/ dir");
    let script_path = scripts_dir.join("version.sh");
    fs::write(&script_path, &filled).expect("write filled version.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
            .expect("chmod version.sh");
    }
    fs::write(
        sandbox.path().join("package.json"),
        "{\n  \"name\": \"bump-md-doc-lint-fixture\",\n  \"version\": \"1.0.0\"\n}\n",
    )
    .expect("write fixture package.json");

    let git = |args: &[&str]| -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(sandbox.path())
            .output()
            .unwrap_or_else(|e| panic!("run `git {args:?}`: {e}"))
    };
    assert!(git(&["init", "-q"]).status.success(), "git init failed");
    assert!(
        git(&["config", "user.email", "bump-md-doc-lint@example.com"])
            .status
            .success(),
        "git config user.email failed"
    );
    assert!(
        git(&["config", "user.name", "bump-md-doc-lint"])
            .status
            .success(),
        "git config user.name failed"
    );

    let run = |args: &[&str]| -> std::process::Output {
        Command::new("bash")
            .arg(&script_path)
            .args(args)
            .current_dir(sandbox.path())
            .output()
            .unwrap_or_else(|e| panic!("run extracted version.sh {args:?}: {e}"))
    };

    // `show` (and bare invocation): prints the current version.
    let out = run(&[]);
    assert!(
        out.status.success(),
        "version.sh (bare) failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "1.0.0",
        "version.sh (bare/show) must print the current version read via get_version()"
    );

    // `check`: all files agree with get_version().
    let out = run(&["check"]);
    assert!(
        out.status.success(),
        "version.sh check failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("All versions in sync: 1.0.0"),
        "version.sh check must report all versions in sync — got: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // `bump patch`: X.Y.Z -> X.Y.(Z+1), rewriting package.json in place.
    let out = run(&["bump", "patch"]);
    assert!(
        out.status.success(),
        "version.sh bump patch failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Version set to 1.0.1"),
        "version.sh bump patch must compute 1.0.1 from 1.0.0 — got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let package_json = fs::read_to_string(sandbox.path().join("package.json"))
        .expect("read package.json after bump");
    assert!(
        package_json.contains("\"1.0.1\""),
        "version.sh bump patch must rewrite package.json's version field — got: {package_json}"
    );

    // `set <version> --tag`: rewrites, stages the VERSION_FILES entries,
    // commits, and creates an annotated tag — the subcommand grammar AC6
    // documents.
    let out = run(&["set", "2.0.0", "--tag"]);
    assert!(
        out.status.success(),
        "version.sh set 2.0.0 --tag failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Created commit and tag v2.0.0"),
        "version.sh set <version> --tag must report the created commit and tag — got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let tags = git(&["tag", "--list", "v2.0.0"]);
    assert_eq!(
        String::from_utf8_lossy(&tags.stdout).trim(),
        "v2.0.0",
        "version.sh set --tag must create annotated tag v2.0.0 via do_tag()"
    );
    let log = git(&["log", "-1", "--oneline"]);
    assert!(
        String::from_utf8_lossy(&log.stdout).contains("bump version to 2.0.0"),
        "version.sh set --tag's do_tag() must commit with message \
         `chore: bump version to <version>` — got: {}",
        String::from_utf8_lossy(&log.stdout)
    );
}

/// AC7: CHANGELOG handling must ensure `## [Unreleased]` and offer to
/// scaffold a Keep-a-Changelog header when CHANGELOG.md is absent.
#[test]
fn bump_md_documents_changelog_handling() {
    let content = read_bump_md();
    // CONTRACT (all three): `## [Unreleased]` and `YYYY-MM-DD` are exact
    // Keep-a-Changelog structural tokens; `Keep a Changelog`/`keepachangelog`
    // names the convention. These are format identifiers, not editable prose.
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
    // CONTRACT (structural): `Phase 7` (by number) + `OPTIONAL` is the
    // confirmation-gate marker; `gh release create` and the `npm publish`
    // disclaimer are exact command/guardrail tokens. The Phase-7 check asserts
    // the phase by NUMBER (prose-tolerant) but the OPTIONAL/command/guardrail
    // tokens are load-bearing contracts — keep EXACT.
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
    // CONTRACT: `/repo:release` is an exact skill reference; the negative
    // `/loom:release` guards against reintroducing the retired skill (#3563).
    // Keep both EXACT.
    assert!(
        content.contains("/repo:release"),
        "bump.md must reference `/repo:release` so readers know where the full \
         release methodology lives (rjwalters/repo, #3563)"
    );
    // PROSE (structural presence): `generic` is a single stable self-identifying
    // word, also anchored by the H1 `(generic)` suffix. A one-word presence
    // check tolerates surrounding rewording while failing if the skill stops
    // identifying itself as the generic quick-bump.
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
        // CONTRACT (negative): the skip-list path is an exact identifier; bump.md
        // must NOT appear on it (it ships to consumers). Keep EXACT.
        assert_ne!(
            entry, ".claude/commands/loom/bump.md",
            "bump.md must NOT be on defaults/.loom-internal.list — it is the \
             generic skill that ships to consumers. #3468 AC1 requires this."
        );
    }
}
