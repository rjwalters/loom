//! Parity tests for the resync-managed *surface* allowlist (#9345).
//!
//! `defaults/scripts/resync-installed.sh` decides which paths its own run
//! wrote; `defaults/scripts/land-resync-commit.sh` decides which paths it is
//! willing to commit. Those two lists must name the same surfaces — but they
//! are two hand-maintained `case` statements in two shell files, and they
//! silently drifted: `resync-installed.sh` grew `.agents/skills/*` (#8673),
//! the managed `.gitignore` block refresh (#4280) and the nested
//! `biome.jsonc` payloads (#6031) without `land-resync-commit.sh`'s mirror
//! following. The observable failure was `land-resync-commit.sh` refusing its
//! own sibling's ordinary output as "non-resync dirt" on every repo where
//! those surfaces existed — i.e. all of them — so the documented one-command
//! land path required manual intervention instead.
//!
//! These tests are the forcing function: adding a surface to one script
//! without the other fails CI here rather than at an operator's terminal.
//! Same reasoning, and same shape, as `credential_class_tests.rs` — shell
//! cannot `source` Rust and a consumer repo has no Rust to ask, so each copy
//! stays literal and the *agreement* is what gets machine-checked.

use std::collections::BTreeSet;

const LAND_RESYNC_COMMIT: &str = include_str!("../../../defaults/scripts/land-resync-commit.sh");
const RESYNC_INSTALLED: &str = include_str!("../../../defaults/scripts/resync-installed.sh");

/// The patterns of the FIRST `case` arm inside `<fn_name>()`.
///
/// Deliberately tolerant of how the two scripts actually spell their arms:
/// one pattern list on a single line, or several `\`-continued lines, with
/// comment lines interleaved between `case ... in` and the arm (both forms
/// occur). Parsing stops at the `)` that closes the pattern list, so comment
/// prose containing parentheses above it cannot confuse the scan.
fn case_patterns(content: &str, fn_name: &str) -> BTreeSet<String> {
    let needle = format!("\n{fn_name}() {{\n");
    let start = content
        .find(&needle)
        .unwrap_or_else(|| panic!("{fn_name}() must be defined (as `{fn_name}() {{`)"));
    let body = &content[start + needle.len()..];

    let mut seen_case = false;
    let mut collected = String::new();
    let mut closed = false;
    for line in body.lines() {
        let t = line.trim();
        if !seen_case {
            assert!(!t.starts_with('}'), "{fn_name}() ended before any `case ... in`");
            if t.starts_with("case ") && t.ends_with(" in") {
                seen_case = true;
            }
            continue;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        collected.push(' ');
        if let Some(idx) = t.find(')') {
            collected.push_str(&t[..idx]);
            closed = true;
            break;
        }
        collected.push_str(t.trim_end_matches('\\'));
    }
    assert!(seen_case, "{fn_name}() must contain a `case ... in`");
    assert!(closed, "{fn_name}()'s first case arm must be closed with `)`");

    collected
        .split('|')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Everything `resync-installed.sh` classifies as its own output: the
/// directory-shaped "pure copy" surfaces plus the single-file cases.
fn resync_installed_surfaces() -> BTreeSet<String> {
    let mut set = case_patterns(RESYNC_INSTALLED, "_is_loom_pure_copy_surface_path");
    set.extend(case_patterns(RESYNC_INSTALLED, "collect_resync_dirt"));
    set
}

#[test]
fn land_allowlist_matches_resync_installed_surfaces() {
    assert_eq!(
        case_patterns(LAND_RESYNC_COMMIT, "is_resync_surface_path"),
        resync_installed_surfaces(),
        "defaults/scripts/land-resync-commit.sh's is_resync_surface_path() has drifted from \
         defaults/scripts/resync-installed.sh's own surface classification \
         (_is_loom_pure_copy_surface_path + collect_resync_dirt). A surface resync writes but \
         land refuses is an operator-visible break in the documented land path (#9345); a \
         surface land accepts but resync never writes lets unrelated dirt into a \
         `chore: resync` commit. Keep the two lists identical."
    );
}

#[test]
fn the_two_surfaces_9345_reported_missing_are_present() {
    // Named explicitly so a future "simplification" of either list cannot
    // quietly drop the pair that caused the incident.
    let land = case_patterns(LAND_RESYNC_COMMIT, "is_resync_surface_path");
    for required in [".agents/skills/*", ".gitignore"] {
        assert!(
            land.contains(required),
            "{required} must stay in land-resync-commit.sh's is_resync_surface_path() (#9345)"
        );
    }
}

/// Does the daemon resync cover the script's surface pattern `pattern`
/// (`.loom/hooks/*`, `.gitignore`)? Covered means one of the roots the
/// payload diff walks, or the stamp it writes itself.
fn daemon_covers(pattern: &str, roots: &[String]) -> bool {
    let path = pattern.trim_end_matches("/*");
    path == crate::install_compat::INSTALL_METADATA_PATH || roots.iter().any(|r| r == path)
}

/// #10895 (the "one table" ask of #8952): every surface `resync-installed.sh`
/// writes is either refreshed by the daemon resync (`payload::surfaces`) or
/// named in its install-time-only list with a reason. A surface added to the
/// script and to neither fails here, instead of silently staying stale in
/// every repo once the script stops being run fleet-wide.
#[test]
fn every_script_surface_is_covered_by_the_daemon_or_declared_install_time_only() {
    use super::payload::surfaces::{EXTRA_SURFACES, INSTALL_TIME_ONLY};

    let defaults = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults");
    let roots = super::payload::resync_roots(&defaults);
    let install_time: BTreeSet<&str> = INSTALL_TIME_ONLY.iter().map(|(p, _)| *p).collect();

    let undecided: Vec<String> = resync_installed_surfaces()
        .into_iter()
        .filter(|p| !daemon_covers(p, &roots) && !install_time.contains(p.as_str()))
        .collect();
    assert!(
        undecided.is_empty(),
        "resync-installed.sh writes {undecided:?}, which the daemon resync neither covers \
         (loom-daemon/src/init/payload/surfaces.rs: EXTRA_SURFACES) nor declares install-time \
         only (INSTALL_TIME_ONLY, with a reason). Decide it in one of the two."
    );

    for (path, reason) in INSTALL_TIME_ONLY {
        assert!(!reason.trim().is_empty(), "{path} needs a reason");
        assert!(
            !roots.iter().any(|r| r == path),
            "{path} is declared install-time only but the daemon resync diffs it"
        );
    }
    // The surfaces #10895 moved under the daemon stay there, and the ones it
    // deliberately left stay declared.
    for covered in [
        ".agents/skills",
        ".gitignore",
        ".loom/CLAUDE.md",
        ".claude/biome.jsonc",
    ] {
        assert!(EXTRA_SURFACES.iter().any(|s| s.path == covered), "{covered}");
    }
    for left in [
        ".loom/config.json",
        "package.json",
        "CLAUDE.md",
        ".gitattributes",
    ] {
        assert!(install_time.contains(left), "{left}");
    }
}

/// The operator-facing copy of the same recipe. `.loom/docs/troubleshooting.md`
/// is a tracked symlink to this file, so an operator on any consumer repo
/// reads (and copy-pastes) exactly these bytes — which makes a stale recipe
/// here just as live a leak path as one the script prints itself.
const TROUBLESHOOTING_DOC: &str = include_str!("../../../defaults/docs/troubleshooting.md");

#[test]
fn no_exclusion_based_staging_recipe_anywhere_an_operator_would_run_one() {
    // #9141: the `--output` next-steps recipe used to be
    // `git add -A -- . ':!.loom/tokens*' …` — an exclusion list, which stages
    // every path nobody thought to list. That is the shape of command behind
    // commit a9da48c2, which swept a whole `.loom/tokens.shadow-disabled-<ts>/`
    // token-pool copy (21 live `.token` files) into a resync commit because
    // the exclusion named `.loom/tokens` and the copy was its sibling. The
    // replacement stages an allowlist of the exact paths the run wrote, so
    // this asserts the exclusion form never comes back — in either of the two
    // places the recipe is published: the script's printed next steps, and
    // the troubleshooting doc's copy-pasteable block.
    //
    // Shell COMMENT lines are exempt for resync-installed.sh only, and only
    // because a line starting with `#` is a comment in its entirety — it
    // cannot be executed, printed, or copy-pasted into a working command, so
    // the retired recipe can still be NAMED there in the prose explaining why
    // it was retired. The doc has no such exemption: every line of it is
    // readable prose an operator may lift.
    for (source, content, allow_shell_comments) in [
        ("defaults/scripts/resync-installed.sh", RESYNC_INSTALLED, true),
        ("defaults/docs/troubleshooting.md", TROUBLESHOOTING_DOC, false),
    ] {
        for (line_no, line) in content.lines().enumerate() {
            if allow_shell_comments && line.trim_start().starts_with('#') {
                continue;
            }
            assert!(
                !line.contains("git add -A -- ."),
                "{}:{} reintroduces an exclusion-based staging recipe \
                 (`git add -A -- . ':!…'`). Stage an allowlist of the paths the run actually \
                 wrote instead — an exclusion list leaks every credential path nobody \
                 listed (#9141).",
                source,
                line_no + 1
            );
        }
    }
}
