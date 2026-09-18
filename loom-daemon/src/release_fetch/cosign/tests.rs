//! Tests for cosign trust-root resolution (epic #7810, PR 6a).

use super::*;

#[test]
fn regex_escape_neutralizes_every_ere_metacharacter() {
    assert_eq!(regex_escape("v0.17.0"), r"v0\.17\.0");
    assert_eq!(regex_escape("a+b*c?"), r"a\+b\*c\?");
    assert_eq!(regex_escape("[x](y){z}|w\\v"), r"\[x\]\(y\)\{z\}\|w\\v");
    // Plain path/slug characters need no escaping.
    assert_eq!(regex_escape("rjwalters/loom"), "rjwalters/loom");
}

/// #5054, the security property `identity_regexp` encodes: a workflow in the
/// SAME repo, at EXACTLY this tag, with the workflow filename left
/// unpinned — a rename of `release.yml` must not hard-abort verification
/// fleet-wide.
#[test]
fn identity_regexp_derives_from_repo_and_tag_with_the_workflow_file_unpinned() {
    let re = identity_regexp("rjwalters/loom", "v0.19.24").unwrap();
    assert_eq!(
        re,
        r"^https://github\.com/rjwalters/loom/\.github/workflows/[^@]+@refs/tags/v0\.19\.24$"
    );
}

#[test]
fn identity_regexp_is_none_without_both_slug_and_tag() {
    assert_eq!(identity_regexp("", "v1.0.0"), None);
    assert_eq!(identity_regexp("rjwalters/loom", ""), None);
}

#[test]
fn oidc_issuer_defaults_to_github_actions() {
    assert_eq!(oidc_issuer(None), "https://token.actions.githubusercontent.com");
    assert_eq!(oidc_issuer(Some("")), "https://token.actions.githubusercontent.com");
}

#[test]
fn oidc_issuer_honors_a_non_empty_override() {
    assert_eq!(oidc_issuer(Some("https://issuer.example")), "https://issuer.example");
}

#[test]
fn pubkey_env_override_wins_when_readable() {
    let dir = tempdir();
    let key = dir.join("my.pub");
    std::fs::write(&key, b"key").unwrap();
    assert_eq!(resolve_pubkey(&dir, Some(key.to_str().unwrap())), Some(key));
}

#[test]
fn pubkey_env_override_is_ignored_when_unreadable() {
    let dir = tempdir();
    // Names a file that does not exist — falls through to the conventional
    // paths, neither of which exist either, so the result is None (a loud
    // skip, never a block).
    assert_eq!(resolve_pubkey(&dir, Some("/nonexistent/cosign.pub")), None);
}

/// #5054's conventional-path branch: no env override, but a checked-in
/// `.loom/cosign.pub` resolves.
#[test]
fn pubkey_falls_back_to_the_conventional_loom_path() {
    let dir = tempdir();
    std::fs::create_dir_all(dir.join(".loom")).unwrap();
    let key = dir.join(".loom").join("cosign.pub");
    std::fs::write(&key, b"key").unwrap();
    assert_eq!(resolve_pubkey(&dir, None), Some(key));
}

#[test]
fn pubkey_falls_back_to_the_conventional_defaults_path() {
    let dir = tempdir();
    std::fs::create_dir_all(dir.join("defaults")).unwrap();
    let key = dir.join("defaults").join("cosign.pub");
    std::fs::write(&key, b"key").unwrap();
    assert_eq!(resolve_pubkey(&dir, None), Some(key));
}

#[test]
fn pubkey_is_none_when_nothing_resolves() {
    let dir = tempdir();
    assert_eq!(resolve_pubkey(&dir, None), None);
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "loom-daemon-cosign-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
