//! Parity tests for the credential-bearing path class (#8005).
//!
//! [`CREDENTIAL_PATTERNS`] is the single declared list. Shell cannot `source`
//! Rust, and a consumer repo has neither `post_init.rs` to parse nor a
//! trustworthy (non-stale) binary to ask, so the two shell stagers carry a
//! literal copy — and these tests are what makes that copy safe: a credential
//! path added on one side of the language boundary but not the other fails
//! here instead of silently leaving one stager unguarded.

use super::post_init::EPHEMERAL_PATTERNS;
use super::{is_credential_path, CREDENTIAL_PATTERNS};
use std::collections::BTreeSet;

const LAND_RESYNC_COMMIT: &str = include_str!("../../../defaults/scripts/land-resync-commit.sh");
const RESYNC_INSTALLED: &str = include_str!("../../../defaults/scripts/resync-installed.sh");

/// The Rust class, normalised the same way both shell copies spell it.
fn rust_class(strip_dir_slash: bool) -> BTreeSet<String> {
    CREDENTIAL_PATTERNS
        .iter()
        .map(|p| {
            if strip_dir_slash {
                p.trim_end_matches('/').to_string()
            } else {
                (*p).to_string()
            }
        })
        .collect()
}

/// Extract `LOOM_CREDENTIAL_PATTERNS=( ... )` (possibly multi-line) from
/// land-resync-commit.sh.
fn land_resync_commit_array() -> BTreeSet<String> {
    let start = LAND_RESYNC_COMMIT
        .find("\nLOOM_CREDENTIAL_PATTERNS=(")
        .expect("land-resync-commit.sh must declare LOOM_CREDENTIAL_PATTERNS=( ... )");
    let body = &LAND_RESYNC_COMMIT[start + "\nLOOM_CREDENTIAL_PATTERNS=(".len()..];
    let end = body
        .find(')')
        .expect("LOOM_CREDENTIAL_PATTERNS array must be closed with `)`");
    body[..end].split_whitespace().map(str::to_string).collect()
}

/// Extract every `':!<path>'` exclusion from resync-installed.sh's printed
/// `git add -A -- . ...` next-steps line.
fn resync_installed_pathspec_excludes() -> BTreeSet<String> {
    let line = RESYNC_INSTALLED
        .lines()
        .find(|l| l.contains("git add -A -- ."))
        .expect("resync-installed.sh must print a `git add -A -- . ':!...'` recipe");
    line.split("':!")
        .skip(1)
        .map(|rest| rest.split('\'').next().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn every_credential_pattern_is_also_gitignored() {
    for p in CREDENTIAL_PATTERNS {
        assert!(
            EPHEMERAL_PATTERNS.contains(p),
            "CREDENTIAL_PATTERNS entry {p:?} is missing from EPHEMERAL_PATTERNS — the \
             .gitignore layer is the first defence and must cover the whole class"
        );
    }
}

#[test]
fn credential_patterns_are_plain_paths_without_globs() {
    // The shared matching contract (dir-with-trailing-slash or exact file) has
    // no glob semantics on either side; a glob here would match differently in
    // Rust and in bash's `[[ == ]]`.
    for p in CREDENTIAL_PATTERNS {
        assert!(
            !p.contains(['*', '?', '[']),
            "CREDENTIAL_PATTERNS entry {p:?} contains a glob character"
        );
        assert!(p.starts_with(".loom/"), "unexpected credential root: {p:?}");
    }
}

#[test]
fn class_covers_the_known_credential_stores() {
    // #3695 token pool + account source, #8401 API-key pool, the harness auth
    // store, #4458/#5401 GH_CONFIG_DIR trees. Removing one of these is a
    // security regression, not a refactor.
    for required in [
        ".loom/tokens/",
        ".loom/accounts.env",
        ".loom/api-keys/",
        ".loom/claude-config/",
        ".loom/gh-config/",
        ".loom/gh-config-by-owner/",
    ] {
        assert!(
            CREDENTIAL_PATTERNS.contains(&required),
            "{required} must stay in CREDENTIAL_PATTERNS"
        );
    }
}

#[test]
fn land_resync_commit_array_matches_rust_class() {
    assert_eq!(
        land_resync_commit_array(),
        rust_class(false),
        "defaults/scripts/land-resync-commit.sh LOOM_CREDENTIAL_PATTERNS has drifted from \
         loom-daemon/src/init/post_init.rs CREDENTIAL_PATTERNS — keep them identical (#8005)"
    );
}

#[test]
fn resync_installed_pathspec_matches_rust_class() {
    assert_eq!(
        resync_installed_pathspec_excludes(),
        rust_class(true),
        "defaults/scripts/resync-installed.sh's printed `git add -A` exclusions have drifted \
         from loom-daemon/src/init/post_init.rs CREDENTIAL_PATTERNS — keep them identical (#8005)"
    );
}

#[test]
fn is_credential_path_matching_contract() {
    // Directory patterns: the dir itself and anything under it.
    assert!(is_credential_path(".loom/tokens"));
    assert!(is_credential_path(".loom/tokens/acct-1.json"));
    assert!(is_credential_path(".loom/gh-config-by-owner/some-owner/hosts.yml"));
    assert!(is_credential_path(".loom/claude-config/builder-1/.credentials.json"));
    // File pattern: exact only.
    assert!(is_credential_path(".loom/accounts.env"));
    assert!(!is_credential_path(".loom/accounts.env.example"));
    assert!(!is_credential_path(".loom/accounts.json"));
    // Prefix-lookalikes must not match a directory pattern.
    assert!(!is_credential_path(".loom/tokens-archive/x"));
    assert!(!is_credential_path(".loom/gh-configx"));
    // Deliberately outside the class (see CREDENTIAL_PATTERNS docs).
    assert!(!is_credential_path(".loom/account-health.json"));
    assert!(!is_credential_path(".loom-local/local.json"));
    assert!(!is_credential_path(".loom/hooks/foo.sh"));
}
