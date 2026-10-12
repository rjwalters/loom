//! Tests for the version-bump-only base-move exemption (#11330).
//!
//! The fixture is `8dffcf1f8` (`chore: bump version to 0.19.1080`), the shape
//! every post-merge bump on `main` has: seven version-bearing files, one
//! parent, authored by the fleet App bot.
#![allow(clippy::unwrap_used)]

use super::super::inputs::{BaseMove, ScopedEvidence};
use super::super::{assess_with_bumps, CheckRun, Verdict};
use super::*;
use std::collections::HashMap;

const BOT: &str = "loom-fleet-dispatch[bot]";
const PARENT: &str = "7e2ee3fd7b41f4020819676a0da3a88185edc0d8";
const BUMP: &str = "8dffcf1f84501edd563a2cd559af58bbe37d9ccd";
/// The real merge before the bump landed at 23:47; the bump at 23:48.
const PARENT_AT: &str = "2026-10-11T23:47:00Z";
const BUMP_AT: &str = "2026-10-11T23:48:22Z";
/// The PR's required check started between the two.
const CHECK_STARTED: &str = "2026-10-11T23:47:30Z";
const CHECK: &str = "Consumer Gate";

fn file(path: &str, patch: &str) -> ChangedFile {
    ChangedFile {
        path: path.to_string(),
        status: "modified".to_string(),
        previous_filename: None,
        patch: Some(patch.to_string()),
    }
}

const CARGO_LOCK_PATCH: &str = "@@ -1257,7 +1257,7 @@\n \n [[package]]\n name = \"loom-api\"\n-version = \"0.19.1079\"\n+version = \"0.19.1080\"\n dependencies = [\n  \"anyhow\",\n@@ -1275,7 +1275,7 @@\n \n [[package]]\n name = \"loom-daemon\"\n-version = \"0.19.1079\"\n+version = \"0.19.1080\"\n dependencies = [\n  \"anyhow\",";

const PKG_BUMP: &str = "@@ -1,6 +1,6 @@\n {\n   \"name\": \"loom\",\n-  \"version\": \"0.19.1079\",\n+  \"version\": \"0.19.1080\",\n   \"description\": \"x\",";

fn bump_files() -> Vec<ChangedFile> {
    vec![
        file(
            ".loom/install-metadata.json",
            "@@ -1 +1 @@\n-  \"loom_version\": \"0.19.1079\",\n+  \"loom_version\": \"0.19.1080\",",
        ),
        file("Cargo.lock", CARGO_LOCK_PATCH),
        file(
            "Cargo.toml",
            "@@ -8,7 +8,7 @@ resolver = \"2\"\n [workspace.package]\n-version = \"0.19.1079\"\n+version = \"0.19.1080\"\n # comment",
        ),
        file("VERSION", "@@ -1 +1 @@\n-0.19.1079\n+0.19.1080"),
        file(
            "mcp-loom/package-lock.json",
            "@@ -1,12 +1,12 @@\n {\n   \"name\": \"@loom/mcp\",\n-  \"version\": \"0.19.1079\",\n+  \"version\": \"0.19.1080\",",
        ),
        file(
            "mcp-loom/package.json",
            PKG_BUMP,
        ),
        file("package.json", PKG_BUMP),
    ]
}

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

fn bump_commit(files: Vec<ChangedFile>) -> BaseCommit {
    BaseCommit {
        sha: BUMP.to_string(),
        author_login: Some(BOT.to_string()),
        author_is_bot: true,
        parents: vec![PARENT.to_string()],
        committed_at: Some(at(BUMP_AT)),
        files,
    }
}

fn parent_commit() -> BaseCommit {
    BaseCommit {
        sha: PARENT.to_string(),
        author_login: Some("turian".to_string()),
        author_is_bot: false,
        parents: vec!["1111111111111111111111111111111111111111".to_string()],
        committed_at: Some(at(PARENT_AT)),
        files: vec![file("loom-daemon/src/lib.rs", "@@ -1 +1 @@\n-a\n+b")],
    }
}

fn fleet(login: &str) -> bool {
    crate::forge_identity::FleetLogins::single("loom-fleet-dispatch").contains(login)
}

fn tail_over(tip: BaseCommit) -> Option<BumpTail> {
    let mut chain: HashMap<String, BaseCommit> = HashMap::new();
    chain.insert(PARENT.to_string(), parent_commit());
    chain.insert(tip.sha.clone(), tip);
    walk(
        BUMP,
        &mut |sha| chain.get(sha).cloned().ok_or_else(|| "404".to_string()),
        &fleet,
    )
}

fn green_run() -> Vec<CheckRun> {
    vec![CheckRun {
        name: CHECK.to_string(),
        status: "completed".to_string(),
        conclusion: Some("success".to_string()),
        started_at: Some(at(CHECK_STARTED)),
        actions_run_id: None,
        actions_job_id: None,
    }]
}

fn assess_time_rule(tail: Option<&BumpTail>) -> Verdict {
    assess_with_bumps(at(BUMP_AT), &[CHECK.to_string()], &green_run(), None, tail).0
}

// --- The acceptance pair ------------------------------------------------------

#[test]
fn a_base_moved_only_by_a_version_bump_is_clean() {
    // Without the exemption the time rule refuses: the check started before
    // the bump landed.
    assert!(matches!(assess_time_rule(None), Verdict::Stale { .. }));
    let tail = tail_over(bump_commit(bump_files())).expect("a pure bump is discounted");
    assert_eq!(tail.effective_sha, PARENT);
    assert_eq!(tail.bumps, 1);
    assert_eq!(assess_time_rule(Some(&tail)), Verdict::Fresh);
}

#[test]
fn a_bump_that_also_touches_changelog_is_still_stale() {
    let mut files = bump_files();
    files.push(file("CHANGELOG.md", "@@ -1 +1,2 @@\n # Changelog\n+- 0.19.1080"));
    assert_eq!(tail_over(bump_commit(files)), None);
    assert!(matches!(assess_time_rule(None), Verdict::Stale { .. }));
}

#[test]
fn a_tested_base_equal_to_the_effective_tip_is_fresh_on_the_scoped_path() {
    // An unknown context is stale on ANY base move (#9589) — here a lockfile
    // move the restamp strip did not discount — unless that move is bumps only.
    let mut ev = ScopedEvidence::default();
    let mut files = super::super::inputs::FileSet::default();
    files.paths.insert("Cargo.lock".to_string());
    ev.base_moves.insert(
        CHECK.to_string(),
        BaseMove {
            tested_base: PARENT[..9].to_string(),
            files,
            ci_scope: Default::default(),
        },
    );
    let required = [CHECK.to_string()];
    let without = assess_with_bumps(at(BUMP_AT), &required, &green_run(), Some(&ev), None).0;
    assert!(matches!(without, Verdict::StaleInputs { .. }), "{without:?}");
    let tail = tail_over(bump_commit(bump_files())).unwrap();
    let with = assess_with_bumps(at(BUMP_AT), &required, &green_run(), Some(&ev), Some(&tail)).0;
    assert_eq!(with, Verdict::Fresh);
}

// --- Each clause refuses on its own -----------------------------------------

#[test]
fn identity_is_the_forge_account_and_must_be_a_fleet_bot() {
    let mut c = bump_commit(bump_files());
    c.author_login = Some("turian".to_string());
    assert!(!is_version_bump(&c, &fleet));
    let mut c = bump_commit(bump_files());
    c.author_is_bot = false;
    assert!(!is_version_bump(&c, &fleet), "a user named like the bot is not the bot");
    let mut c = bump_commit(bump_files());
    c.author_login = None;
    assert!(!is_version_bump(&c, &fleet));
    assert!(is_version_bump(&bump_commit(bump_files()), &fleet));
}

#[test]
fn a_merge_commit_or_a_version_decrease_is_not_a_bump() {
    let mut c = bump_commit(bump_files());
    c.parents.push(PARENT.to_string());
    assert!(!is_version_bump(&c, &fleet));
    let mut files = bump_files();
    for f in &mut files {
        let p = f.patch.take().unwrap();
        f.patch = Some(
            p.replace("1079", "XXXX")
                .replace("1080", "1079")
                .replace("XXXX", "1080"),
        );
    }
    assert!(!is_version_bump(&bump_commit(files), &fleet));
}

#[test]
fn a_dependency_moving_in_cargo_lock_is_not_a_bump() {
    let dep = "@@ -10,7 +10,7 @@\n \n [[package]]\n name = \"serde\"\n-version = \"0.19.1079\"\n+version = \"0.19.1080\"\n source = \"registry+https://github.com/rust-lang/crates.io-index\"\n checksum = \"x\"";
    assert!(!cargo_lock_is_workspace_only(dep));
    assert!(cargo_lock_is_workspace_only(CARGO_LOCK_PATCH));
    let mut files = bump_files();
    files[1] = file("Cargo.lock", &format!("{CARGO_LOCK_PATCH}\n{dep}"));
    assert!(!is_version_bump(&bump_commit(files), &fleet));
    // A version line with no readable neighbour is an unknown.
    assert!(!cargo_lock_is_workspace_only("@@ -1 +1 @@\n-version = \"1\"\n+version = \"2\""));
}

#[test]
fn a_tip_that_is_not_a_bump_or_an_unreadable_walk_discounts_nothing() {
    let mut chain: HashMap<String, BaseCommit> = HashMap::new();
    chain.insert(PARENT.to_string(), parent_commit());
    assert_eq!(walk(PARENT, &mut |s| chain.get(s).cloned().ok_or_default(), &fleet), None);
    // The bump is readable, its parent is not: no effective tip, no discount.
    let bump = bump_commit(bump_files());
    assert_eq!(
        walk(
            BUMP,
            &mut |s| if s == BUMP {
                Ok(bump.clone())
            } else {
                Err("boom".to_string())
            },
            &fleet
        ),
        None
    );
}

trait OkOrDefault<T> {
    fn ok_or_default(self) -> Result<T, String>;
}
impl<T> OkOrDefault<T> for Option<T> {
    fn ok_or_default(self) -> Result<T, String> {
        self.ok_or_else(|| "404".to_string())
    }
}

// --- No second copy of the version set ---------------------------------------

#[test]
fn the_version_set_is_version_sh_list_plus_lockfiles() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let out = std::process::Command::new("bash")
        .args(["scripts/version.sh", "list"])
        .current_dir(&root)
        .output()
        .expect("scripts/version.sh must be runnable");
    assert!(out.status.success(), "{out:?}");
    let set = version_set();
    for path in String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
    {
        assert!(set.iter().any(|p| p == path), "version.sh lists {path}; set is {set:?}");
    }
    // install-metadata is listed only when present, so pin it explicitly, plus
    // the lockfiles the bump workflow stages beside the manifests.
    for path in [
        ".loom/install-metadata.json",
        "Cargo.lock",
        "mcp-loom/package-lock.json",
    ] {
        assert!(set.iter().any(|p| p == path), "{path} missing from {set:?}");
    }
    assert!(!set.iter().any(|p| p == "CHANGELOG.md"));
}

#[test]
fn commit_json_projects_the_fields_the_rule_reads() {
    let body = serde_json::json!({
        "sha": BUMP,
        "author": {"login": BOT, "type": "Bot"},
        "parents": [{"sha": PARENT}],
        "commit": {"committer": {"date": BUMP_AT}},
        "files": [{"filename": "VERSION", "status": "modified", "patch": "@@ -1 +1 @@\n-0.19.1079\n+0.19.1080"}]
    })
    .to_string();
    let c = commit_from_json(&body).unwrap();
    assert_eq!(c.author_login.as_deref(), Some(BOT));
    assert!(c.author_is_bot);
    assert_eq!(c.parents, vec![PARENT.to_string()]);
    assert_eq!(c.committed_at, Some(at(BUMP_AT)));
    assert_eq!(c.files.len(), 1);
    assert!(commit_from_json("{\"sha\": \"x\"}").is_err(), "no files array is an unknown");
}

// --- The bump must change ONLY the release-owned version values (#11538) -----

/// `bump_files()` with the file at `path` replaced by `patch`.
fn bump_with(path: &str, patch: &str) -> BaseCommit {
    let mut files = bump_files();
    let i = files
        .iter()
        .position(|f| f.path == path)
        .expect("fixture file");
    files[i] = file(path, patch);
    bump_commit(files)
}

#[test]
fn a_changed_build_command_carrying_the_versions_is_not_a_bump() {
    let patch = "@@ -1,6 +1,6 @@\n {\n   \"name\": \"loom\",\n-  \"build\": \"node build.js --version 0.19.1079\",\n+  \"build\": \"node different-build.js --version 0.19.1080\",";
    assert!(!is_version_bump(&bump_with("package.json", patch), &fleet));
    assert_eq!(tail_over(bump_with("package.json", patch)), None);
}

#[test]
fn a_dependency_edit_in_a_json_manifest_or_lockfile_is_not_a_bump() {
    // A dependency named like the field, and a lockfile entry's own version.
    let manifest = "@@ -5,6 +5,6 @@\n   \"dependencies\": {\n-    \"version\": \"0.19.1079\",\n+    \"version\": \"0.19.1080\",";
    assert!(!is_version_bump(&bump_with("package.json", manifest), &fleet));
    let dep_value = "@@ -5,6 +5,6 @@\n   \"name\": \"loom\",\n-  \"left-pad\": \"0.19.1079\",\n+  \"left-pad\": \"0.19.1080\",";
    assert!(!is_version_bump(&bump_with("package.json", dep_value), &fleet));
    let lock = "@@ -20,7 +20,7 @@\n     \"node_modules/left-pad\": {\n-      \"version\": \"0.19.1079\",\n+      \"version\": \"0.19.1080\",";
    assert!(!is_version_bump(&bump_with("mcp-loom/package-lock.json", lock), &fleet));
}

#[test]
fn only_the_version_value_may_differ_between_the_removed_and_added_line() {
    // Same field, extra content, indentation, or an added/removed line.
    for patch in [
        "@@ -1,4 +1,4 @@\n {\n   \"name\": \"loom\",\n-  \"version\": \"0.19.1079\",\n+  \"version\": \"0.19.1080\", \"x\": 1,",
        "@@ -1,4 +1,4 @@\n {\n   \"name\": \"loom\",\n-  \"version\": \"0.19.1079\",\n+    \"version\": \"0.19.1080\",",
        "@@ -1,4 +1,5 @@\n {\n   \"name\": \"loom\",\n-  \"version\": \"0.19.1079\",\n+  \"version\": \"0.19.1080\",\n+  \"private\": false,",
        "@@ -1,4 +1,3 @@\n {\n   \"name\": \"loom\",\n-  \"version\": \"0.19.1079\",",
        // No context above: the field's owner is unknown.
        "@@ -1 +1 @@\n-  \"version\": \"0.19.1079\",\n+  \"version\": \"0.19.1080\",",
    ] {
        assert!(!is_version_bump(&bump_with("package.json", patch), &fleet), "{patch}");
    }
}

#[test]
fn a_cargo_toml_edit_outside_the_package_version_is_not_a_bump() {
    let dep = "@@ -20,7 +20,7 @@\n [workspace.dependencies]\n-version = \"0.19.1079\"\n+version = \"0.19.1080\"";
    assert!(!is_version_bump(&bump_with("Cargo.toml", dep), &fleet));
    let other = "@@ -8,7 +8,7 @@\n [workspace.package]\n-edition = \"2021\"\n+edition = \"2024\"";
    assert!(!is_version_bump(&bump_with("Cargo.toml", other), &fleet));
}

#[test]
fn a_foreign_field_in_install_metadata_or_cargo_lock_is_not_a_bump() {
    let meta = "@@ -1,4 +1,4 @@\n {\n-  \"loom_commit\": \"0.19.1079\",\n+  \"loom_commit\": \"0.19.1080\",";
    assert!(!is_version_bump(&bump_with(".loom/install-metadata.json", meta), &fleet));
    let lock = "@@ -1,7 +1,7 @@\n [[package]]\n name = \"loom-api\"\n-version = \"0.19.1079\"\n+version = \"0.19.1080-rc\"\n dependencies = [";
    assert!(!is_version_bump(&bump_with("Cargo.lock", lock), &fleet));
}
