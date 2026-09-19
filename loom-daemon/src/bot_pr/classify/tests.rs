use super::*;
use crate::bot_pr::config::{from_value, MaxSemver};
use serde_json::json;

fn enabled() -> BotPrConfig {
    from_value(&json!({ "champion": { "autoMergeDependabot": true } }))
}

fn input(author: &str, files: &[&str]) -> ClassifyInput {
    ClassifyInput {
        author: author.to_string(),
        title: "Bump serde from 1.0.1 to 1.0.2".to_string(),
        body: String::new(),
        files: files.iter().map(|s| (*s).to_string()).collect(),
        patches: HashMap::new(),
    }
}

// --- AC6: flag off => behavior identical to today ---------------------------

#[test]
fn the_flag_off_disqualifies_even_a_perfect_dependabot_pr() {
    let cfg = BotPrConfig::default();
    let err = classify(&cfg, &input("dependabot[bot]", &["Cargo.lock"])).unwrap_err();
    assert_eq!(err, Disqualified::Disabled);
    assert_eq!(err.code(), "disabled");
}

// --- AC1: author check ------------------------------------------------------

#[test]
fn a_trusted_bot_with_a_lockfile_only_diff_qualifies() {
    let q = classify(&enabled(), &input("dependabot[bot]", &["Cargo.lock"])).unwrap();
    assert_eq!(q.waived, ["critical-file", "recency-doctor-route"]);
    assert_eq!(q.bump_level, None, "no ceiling set => nothing evaluated");
}

#[test]
fn a_human_pushing_the_same_diff_does_not_qualify() {
    // Spoof resistance: the verdict is a function of author.login only — never
    // the branch name, never the title, both of which a human can forge.
    let err = classify(&enabled(), &input("rjwalters", &["Cargo.lock"])).unwrap_err();
    assert_eq!(err.code(), "untrusted-author");
    assert!(err.explain().contains("rjwalters"));
}

#[test]
fn renovate_qualifies_only_when_the_repo_lists_it() {
    let base = enabled();
    assert_eq!(
        classify(&base, &input("renovate[bot]", &["package.json"]))
            .unwrap_err()
            .code(),
        "untrusted-author"
    );
    let widened = from_value(&json!({
        "champion": {
            "autoMergeDependabot": true,
            "trustedBotAuthors": ["dependabot[bot]", "renovate[bot]"]
        }
    }));
    assert!(classify(&widened, &input("renovate[bot]", &["package.json"])).is_ok());
}

// --- AC2/AC3: dependency-only diff, and the fallback --------------------------

#[test]
fn a_multi_manifest_diff_qualifies() {
    let q = classify(
        &enabled(),
        &input(
            "dependabot[bot]",
            &[
                "Cargo.toml",
                "Cargo.lock",
                "mcp-loom/package.json",
                "mcp-loom/package-lock.json",
            ],
        ),
    )
    .unwrap();
    assert_eq!(q.waived, WAIVED_CRITERIA);
}

#[test]
fn any_non_manifest_file_falls_back_to_the_strict_criteria() {
    let err = classify(&enabled(), &input("dependabot[bot]", &["Cargo.lock", "src/main.rs"]))
        .unwrap_err();
    assert_eq!(
        err,
        Disqualified::NonManifestFile {
            path: "src/main.rs".to_string()
        }
    );
    assert!(err.explain().contains("src/main.rs"));
}

#[test]
fn a_bot_pr_touching_its_own_dependabot_config_does_not_qualify() {
    let err =
        classify(&enabled(), &input("dependabot[bot]", &[".github/dependabot.yml"])).unwrap_err();
    assert_eq!(err.code(), "non-manifest-file");
}

#[test]
fn a_pr_with_no_files_cannot_be_shown_to_be_dependency_only() {
    assert_eq!(
        classify(&enabled(), &input("dependabot[bot]", &[])).unwrap_err(),
        Disqualified::NoFiles
    );
    assert_eq!(
        classify(&enabled(), &input("dependabot[bot]", &["  "])).unwrap_err(),
        Disqualified::NoFiles
    );
}

// --- The operator ruling's workflow `uses:` carve-out (2026-09-14) -----------

fn with_workflow_patch(patch: &str) -> ClassifyInput {
    let mut i = input("dependabot[bot]", &[".github/workflows/ci.yml"]);
    i.title = "Bump actions/checkout from 4 to 5".to_string();
    i.patches
        .insert(".github/workflows/ci.yml".to_string(), patch.to_string());
    i
}

#[test]
fn a_workflow_uses_pin_bump_qualifies() {
    let i = with_workflow_patch(
        "@@ -1,1 +1,1 @@\n-      - uses: actions/checkout@v4\n+      - uses: actions/checkout@v5\n",
    );
    assert!(classify(&enabled(), &i).is_ok());
}

#[test]
fn a_workflow_change_beyond_a_version_pin_does_not_qualify() {
    let i = with_workflow_patch(
        "@@ -1,1 +1,2 @@\n-      - uses: actions/checkout@v4\n+      - uses: actions/checkout@v5\n+      - run: rm -rf /\n",
    );
    assert_eq!(classify(&enabled(), &i).unwrap_err().code(), "workflow-not-version-pin-only");
}

#[test]
fn a_workflow_with_no_patch_available_fails_closed() {
    let mut i = input("dependabot[bot]", &[".github/workflows/ci.yml"]);
    i.patches.clear();
    let err = classify(&enabled(), &i).unwrap_err();
    assert_eq!(err.code(), "workflow-patch-unavailable");
    assert!(err.explain().contains(".github/workflows/ci.yml"));
}

// --- AC5: dependabot_max_semver ----------------------------------------------

fn with_ceiling(ceiling: &str) -> BotPrConfig {
    from_value(&json!({
        "champion": { "autoMergeDependabot": true, "dependabotMaxSemver": ceiling }
    }))
}

#[test]
fn the_ceiling_admits_a_bump_at_or_below_it() {
    let mut i = input("dependabot[bot]", &["Cargo.lock"]);
    i.title = "Bump serde from 1.0.1 to 1.1.0".to_string();
    let q = classify(&with_ceiling("minor"), &i).unwrap();
    assert_eq!(q.bump_level, Some(BumpLevel::Minor));
}

#[test]
fn the_ceiling_rejects_a_bump_above_it() {
    let mut i = input("dependabot[bot]", &["Cargo.lock"]);
    i.title = "Bump serde from 1.0.1 to 2.0.0".to_string();
    let err = classify(&with_ceiling("minor"), &i).unwrap_err();
    assert_eq!(
        err,
        Disqualified::SemverExceeded {
            level: BumpLevel::Major,
            max: "minor"
        }
    );
    assert!(err.explain().contains("major"));
}

#[test]
fn a_grouped_pr_qualifies_only_when_every_member_bump_does() {
    let mut i = input("dependabot[bot]", &["Cargo.lock"]);
    i.title = "Bump the cargo group with 2 updates".to_string();
    i.body = "Updates `a` from 1.0.0 to 1.0.1\nUpdates `b` from 1.0.0 to 1.1.0\n".to_string();
    assert!(classify(&with_ceiling("minor"), &i).is_ok());

    i.body = "Updates `a` from 1.0.0 to 1.0.1\nUpdates `b` from 1.0.0 to 2.0.0\n".to_string();
    assert_eq!(classify(&with_ceiling("minor"), &i).unwrap_err().code(), "semver-exceeded");
}

#[test]
fn an_unparseable_title_under_a_ceiling_fails_closed() {
    let mut i = input("dependabot[bot]", &["Cargo.lock"]);
    i.title = "Update lockfile".to_string();
    assert_eq!(classify(&with_ceiling("patch"), &i).unwrap_err().code(), "semver-unparseable");
    // ...but with no ceiling set (the default) the same PR is fine: CI-green
    // is the gate, not the title's grammar.
    assert!(classify(&enabled(), &i).is_ok());
}

#[test]
fn the_default_ceiling_is_all() {
    assert_eq!(enabled().max_semver, MaxSemver::All);
}

// --- Ordering: the cheapest, most-decisive check first ------------------------

#[test]
fn author_is_checked_before_files_so_a_human_pr_never_reports_a_file_reason() {
    let err = classify(&enabled(), &input("rjwalters", &["src/main.rs"])).unwrap_err();
    assert_eq!(err.code(), "untrusted-author");
}
