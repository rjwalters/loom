//! End to end: a base move that is only machine restamps never stales a
//! required check (#10163).
//!
//! `tests.rs` pins [`strip_validated_restamps`] on its own. The 2026-10-04
//! chain-head livelock (#9832) suspected the post-merge version bump and the
//! resync stamp of re-staling the head, so these tests run the whole #8919
//! decision: the discount, then the input-scoped predicate over EVERY required
//! context. The patches are copied byte for byte from real `main` commits,
//! context lines included: `bd6d9bb93` (`chore: bump version to 0.19.660`, the
//! bump that followed #10153 inside the livelock window) and `d406ca56a`
//! (`chore: resync installed Loom surfaces`). A fixture that only "looks
//! right" is how a discount bug would survive.

use super::*;
use crate::merge_pr::stale_checks::inputs::{
    composite_stale_reason, file_set, specs_for, FileSet, REQUIRED_CHECKS,
};
use crate::merge_pr::stale_checks::workflow_scope::CiScopes;

fn modified(path: &str, patch: &str) -> ChangedFile {
    ChangedFile {
        path: path.to_string(),
        status: "modified".to_string(),
        previous_filename: None,
        patch: Some(patch.to_string()),
    }
}

/// The required contexts the guard calls stale for base move `moved` against
/// PR delta `p`, after the restamp discount.
fn stale_contexts(moved: &[ChangedFile], p: &FileSet) -> Vec<String> {
    let d = to_file_set(&strip_validated_restamps(moved));
    REQUIRED_CHECKS
        .iter()
        .filter(|r| {
            let specs = specs_for(r.context).expect("every required context has specs");
            composite_stale_reason(&specs, &d, p, &CiScopes::unscoped()).is_some()
        })
        .map(|r| r.context.to_string())
        .collect()
}

/// A PR delta that touches no gate's inputs.
fn docs_only_pr() -> FileSet {
    file_set([("docs/adr/0020-something.md", false)])
}

/// A PR delta that touches a shell script, so every gate that scans `**/*.sh`
/// (Shell Budget Ratchet in `Daemon Checks`, Shell Syntax in `Structural
/// Checks`) has something to re-judge if a global input moves under it.
fn shell_pr() -> FileSet {
    file_set([("defaults/scripts/merge-pr.sh", false)])
}

const OLD: &str = "0.19.659";
const NEW: &str = "0.19.660";

/// Every file of real bump commit `bd6d9bb93`, versions substituted so a test
/// can model a span of several bumps (`B` several bumps behind the tip).
fn real_bump(old: &str, new: &str) -> Vec<ChangedFile> {
    let s = |t: &str| t.replace("{O}", old).replace("{N}", new);
    vec![
        modified(
            ".loom/install-metadata.json",
            &s("@@ -1,5 +1,5 @@\n {\n-  \"loom_version\": \"{O}\",\n+  \"loom_version\": \"{N}\",\n   \"loom_commit\": \"ea912279f34957843105df946ff191c1945b9060\",\n   \"install_date\": \"2026-04-21\",\n   \"installed_files\": [\n"),
        ),
        modified(
            "Cargo.lock",
            &s("@@ -1257,7 +1257,7 @@ checksum = \"f9f8bd3e56ce4dfc153cf470fffbfa98c7620958b312ca5c3a4b8d5181fd13c6\"\n \n [[package]]\n name = \"loom-api\"\n-version = \"{O}\"\n+version = \"{N}\"\n dependencies = [\n  \"anyhow\",\n  \"axum\",\n@@ -1275,7 +1275,7 @@ dependencies = [\n \n [[package]]\n name = \"loom-daemon\"\n-version = \"{O}\"\n+version = \"{N}\"\n dependencies = [\n  \"anyhow\",\n  \"base64 0.23.1\",\n"),
        ),
        modified(
            "Cargo.toml",
            &s("@@ -8,7 +8,7 @@ resolver = \"2\"\n # hardcoded its own version, so every bump edited two files that could drift\n # apart independently -- the \"the bump missed a file\" class of #6536/#6730.\n [workspace.package]\n-version = \"{O}\"\n+version = \"{N}\"\n # The repo this workspace is built from — compiled into `loom-daemon` via\n # `repository.workspace = true` + `env!(\"CARGO_PKG_REPOSITORY\")` (#8513). This\n # is the build-time fallback `auto_update`'s release resolver falls back to\n"),
        ),
        modified("VERSION", &s("@@ -1 +1 @@\n-{O}\n+{N}\n")),
        modified(
            "mcp-loom/package-lock.json",
            &s("@@ -1,12 +1,12 @@\n {\n   \"name\": \"@loom/mcp\",\n-  \"version\": \"{O}\",\n+  \"version\": \"{N}\",\n   \"lockfileVersion\": 3,\n   \"requires\": true,\n   \"packages\": {\n     \"\": {\n       \"name\": \"@loom/mcp\",\n-      \"version\": \"{O}\",\n+      \"version\": \"{N}\",\n       \"dependencies\": {\n         \"@modelcontextprotocol/sdk\": \"^1.30.1\",\n         \"strip-ansi\": \"^7.2.0\"\n"),
        ),
        modified(
            "mcp-loom/package.json",
            &s("@@ -1,6 +1,6 @@\n {\n   \"name\": \"@loom/mcp\",\n-  \"version\": \"{O}\",\n+  \"version\": \"{N}\",\n   \"description\": \"Unified MCP server for Loom - combines logs, UI, and terminal tools\",\n   \"type\": \"module\",\n   \"engines\": {\n"),
        ),
        modified(
            "package.json",
            &s("@@ -1,6 +1,6 @@\n {\n   \"name\": \"loom\",\n-  \"version\": \"{O}\",\n+  \"version\": \"{N}\",\n   \"description\": \"AI-powered development orchestration.\",\n   \"type\": \"module\",\n   \"engines\": {\n"),
        ),
    ]
}

/// Real resync commit `d406ca56a`: `.loom/install-metadata.json` only.
fn real_resync() -> ChangedFile {
    modified(
        ".loom/install-metadata.json",
        "@@ -1,6 +1,6 @@\n {\n   \"loom_version\": \"0.19.655\",\n-  \"loom_commit\": \"fc667a4faae1227f8a6837431f8a0c2d0aeaa7dd\",\n+  \"loom_commit\": \"ea912279f34957843105df946ff191c1945b9060\",\n   \"install_date\": \"2026-04-21\",\n   \"installed_files\": [\n     \".claude/README.md\",\n",
    )
}

fn replace(moved: &mut Vec<ChangedFile>, file: ChangedFile) {
    moved.retain(|c| c.path != file.path);
    moved.push(file);
}

// --- Positive: restamp-only moves ------------------------------------------

#[test]
fn the_real_bump_commit_is_discounted_file_for_file() {
    let moved = real_bump(OLD, NEW);
    assert_eq!(moved.len(), 7, "the bump rewrites seven files");
    let kept = strip_validated_restamps(&moved);
    assert!(kept.is_empty(), "every file of a real bump is a restamp: {kept:?}");
}

#[test]
fn a_version_bump_only_move_stales_no_required_check() {
    let stale = stale_contexts(&real_bump(OLD, NEW), &docs_only_pr());
    assert!(stale.is_empty(), "a bump-only move must not stale: {stale:?}");
}

#[test]
fn a_version_bump_only_move_stales_nothing_even_for_a_pr_on_gate_inputs() {
    // The discount empties D, so no clause can fire whatever P holds: here P
    // carries a global input of nearly every gate (ci.yml) plus shell scripts.
    let p = file_set([
        ("Cargo.toml", false),
        (".github/workflows/ci.yml", false),
        ("defaults/scripts/merge-pr.sh", false),
    ]);
    let stale = stale_contexts(&real_bump(OLD, NEW), &p);
    assert!(stale.is_empty(), "{stale:?}");
}

#[test]
fn a_span_of_several_bumps_is_still_one_discountable_restamp() {
    // `B` three bumps behind the tip: the compare collapses them to one pair.
    let stale = stale_contexts(&real_bump("0.19.659", "0.19.662"), &shell_pr());
    assert!(stale.is_empty(), "{stale:?}");
}

#[test]
fn a_resync_only_move_stales_no_required_check() {
    for p in [docs_only_pr(), shell_pr()] {
        let stale = stale_contexts(&[real_resync()], &p);
        assert!(stale.is_empty(), "a resync-only move must not stale: {stale:?}");
    }
}

#[test]
fn a_bump_and_resync_in_one_move_stales_no_required_check() {
    // A compare spanning both commits carries one install-metadata entry with
    // both the version pair and the resync field.
    let mut moved = real_bump(OLD, NEW);
    replace(
        &mut moved,
        modified(
            ".loom/install-metadata.json",
            &format!(
                "@@ -1,5 +1,5 @@\n {{\n-  \"loom_version\": \"{OLD}\",\n\
                 -  \"loom_commit\": \"fc667a4faae1227f8a6837431f8a0c2d0aeaa7dd\",\n\
                 +  \"loom_version\": \"{NEW}\",\n\
                 +  \"loom_commit\": \"ea912279f34957843105df946ff191c1945b9060\",\n\
                 \x20  \"install_date\": \"2026-04-21\",\n"
            ),
        ),
    );
    let stale = stale_contexts(&moved, &shell_pr());
    assert!(stale.is_empty(), "{stale:?}");
}

#[test]
fn an_unrelated_merge_plus_its_bump_stales_nothing_for_a_non_overlapping_pr() {
    // The real livelock shape: a merge to main, then its bump. The merge's own
    // files are judged by the ordinary clauses; the bump adds nothing.
    let mut moved = real_bump(OLD, NEW);
    moved.push(modified("docs/guides/some-guide.md", "@@ -1 +1 @@\n-a\n+b\n"));
    let stale = stale_contexts(&moved, &shell_pr());
    assert!(stale.is_empty(), "{stale:?}");
}

// --- Negative: fail closed -------------------------------------------------

#[test]
fn a_bump_that_smuggles_a_dependency_edit_stales_daemon_checks() {
    // A Cargo.lock line carrying some OTHER package's version is a real
    // dependency change. It survives the discount and, as a global input of
    // Shell Budget Ratchet, stales `Daemon Checks` for a PR with a script.
    let mut moved = real_bump(OLD, NEW);
    replace(
        &mut moved,
        modified(
            "Cargo.lock",
            &format!(
                "@@ -1275,7 +1275,7 @@ dependencies = [\n name = \"loom-daemon\"\n\
                 -version = \"{OLD}\"\n+version = \"{NEW}\"\n\
                 @@ -2000,3 +2000,3 @@\n name = \"serde\"\n\
                 -version = \"1.0.3\"\n+version = \"1.0.4\"\n"
            ),
        ),
    );
    let kept = strip_validated_restamps(&moved);
    let kept_paths: Vec<&str> = kept.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(kept_paths, vec!["Cargo.lock"]);
    let stale = stale_contexts(&moved, &shell_pr());
    assert!(stale.contains(&"Daemon Checks".to_string()), "{stale:?}");
}

#[test]
fn a_version_decrease_is_not_a_restamp_and_stales() {
    let moved = real_bump(NEW, OLD);
    let kept = strip_validated_restamps(&moved);
    assert_eq!(kept.len(), moved.len(), "nothing is discounted: {kept:?}");
    let stale = stale_contexts(&moved, &shell_pr());
    // VERSION is a global input of Shell Syntax; Cargo.lock of Shell Budget.
    assert!(stale.contains(&"Structural Checks".to_string()), "{stale:?}");
    assert!(stale.contains(&"Daemon Checks".to_string()), "{stale:?}");
}

#[test]
fn a_bump_without_its_version_file_discounts_nothing() {
    let mut moved = real_bump(OLD, NEW);
    moved.retain(|c| c.path != "VERSION");
    let kept = strip_validated_restamps(&moved);
    // Only install-metadata could ever pass without the pair, and its
    // `loom_version` line still needs it.
    assert_eq!(kept.len(), moved.len(), "{kept:?}");
    assert!(!stale_contexts(&moved, &shell_pr()).is_empty());
}

// --- RESTAMP_PATHS mirrors `scripts/version.sh list` -------------------------

#[test]
fn every_path_version_sh_lists_is_a_restamp_candidate() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let out = std::process::Command::new("bash")
        .arg("scripts/version.sh")
        .arg("list")
        .current_dir(&root)
        .output()
        .expect("scripts/version.sh must be runnable");
    assert!(out.status.success(), "version.sh list failed: {out:?}");
    let listed: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    assert!(listed.len() >= 5, "suspiciously short list: {listed:?}");
    for path in &listed {
        assert!(
            RESTAMP_PATHS.contains(&path.as_str()),
            "`scripts/version.sh list` names `{path}` but RESTAMP_PATHS does not: a release \
             bump to it would never be discounted and every merge would stale open PRs"
        );
    }
}
