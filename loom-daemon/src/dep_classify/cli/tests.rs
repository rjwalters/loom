//! Tests for the CLI boundary (epic #7810, PR 3).
//!
//! Deliberately thin. This layer's behaviour — argv, stdout markers, exit codes
//! and the forge writes — is proven end-to-end by
//! `defaults/scripts/tests/test-classify-dependency-block.sh` and its two
//! siblings, 261 assertions that were written against the shell implementation
//! and now drive this one unchanged. Restating those here would be a second,
//! weaker copy of the same evidence.
//!
//! What IS here is the one thing those suites never exercise: they all pass
//! `--repo` explicitly, so repo resolution never runs.

use super::*;

#[test]
fn an_ssh_remote_yields_owner_repo() {
    assert_eq!(
        nwo_from_remote_url("git@github.com:acme/widgets.git"),
        Some("acme/widgets".to_string())
    );
}

#[test]
fn an_https_remote_yields_owner_repo() {
    assert_eq!(
        nwo_from_remote_url("https://github.com/acme/widgets.git"),
        Some("acme/widgets".to_string())
    );
}

#[test]
fn the_git_suffix_is_optional() {
    assert_eq!(
        nwo_from_remote_url("https://github.com/acme/widgets"),
        Some("acme/widgets".to_string())
    );
}

#[test]
fn a_self_hosted_host_with_a_port_path_still_yields_the_last_two_segments() {
    assert_eq!(
        nwo_from_remote_url("https://git.example.com/acme/widgets.git"),
        Some("acme/widgets".to_string())
    );
}

#[test]
fn a_url_that_cannot_name_both_halves_is_none_not_a_guess() {
    // Falls through to `gh repo view` rather than inventing an owner.
    assert_eq!(nwo_from_remote_url("widgets"), None);
    assert_eq!(nwo_from_remote_url(""), None);
    assert_eq!(nwo_from_remote_url("https://github.com/"), None);
}

#[test]
fn a_checkout_with_an_origin_remote_resolves_without_asking_the_forge() {
    // The reason git comes first: this answer is available offline and under
    // API exhaustion, which is exactly when a Champion pass most needs to keep
    // running.
    let dir = std::env::temp_dir().join(format!(
        "loom-dep-classify-repo-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(args)
            .output()
            .expect("git")
    };
    git(&["init", "-q"]);
    git(&["remote", "add", "origin", "git@github.com:acme/widgets.git"]);

    let got = resolve_repo(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(got, Some("acme/widgets".to_string()));
}

#[test]
fn forge_calls_are_made_from_the_repo_root_not_the_process_cwd() {
    // `gh_cmd` looks for the read cache at `<dir>/.loom/scripts/gh-cached`.
    // Resolving that from the process cwd loses the cache for any caller
    // running from a subdirectory — correct answers, but one uncached `gh`
    // call per blocker per pass, against a module that promises the opposite.
    let dir = std::env::temp_dir().join(format!(
        "loom-forge-dir-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let nested = dir.join("a").join("b");
    std::fs::create_dir_all(&nested).expect("dirs");
    std::fs::create_dir_all(dir.join(".loom")).expect(".loom");
    std::fs::create_dir_all(dir.join(".git")).expect(".git");

    let got = forge_dir(&nested);
    let want = dir.canonicalize().unwrap_or(dir.clone());
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        got.canonicalize().unwrap_or(got),
        want,
        "a call from a subdirectory must still find the repo's gh-cached"
    );
}

#[test]
fn a_directory_outside_any_repository_is_used_unchanged() {
    // No repo root to find: fall back rather than fail. The forge call still
    // works, it just goes through plain `gh` with no cache — which is exactly
    // what the shell did when its own probe came up empty.
    let dir = std::env::temp_dir();
    assert_eq!(forge_dir(&dir), dir);
}
