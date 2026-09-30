//! Parity tests for the credential-bearing path class (#8005, extended by the
//! #8734 audit).
//!
//! [`CREDENTIAL_PATTERNS`] is the single declared list. Shell cannot `source`
//! Rust, and a consumer repo has neither `post_init.rs` to parse nor a
//! trustworthy (non-stale) binary to ask, so every shell stager that needs
//! the class carries a literal copy — and these tests are what makes each
//! copy safe: a credential path added on one side of the language boundary
//! but not the other fails here instead of silently leaving a stager
//! unguarded. Covers `defaults/scripts/land-resync-commit.sh`,
//! `scripts/install-loom.sh`, and `install.sh`.
//!
//! `defaults/scripts/resync-installed.sh` used to carry a copy too — the
//! `':!…'` exclusion list on the `git add -A` recipe its `--output` mode
//! printed. #9141 removed that recipe entirely in favour of an allowlist of
//! the paths the run actually wrote, so the script no longer needs to know
//! what a credential path looks like: an allowlist has no "everything else"
//! to leak. `resync_surface_parity_tests.rs` asserts the exclusion form does
//! not come back.

use super::post_init::EPHEMERAL_PATTERNS;
use super::{is_credential_path, CREDENTIAL_PATTERNS};
use std::collections::BTreeSet;

const LAND_RESYNC_COMMIT: &str = include_str!("../../../defaults/scripts/land-resync-commit.sh");
// #8734: the two "initial commit" `git add -A` sites audited alongside
// land-resync-commit.sh above. Unlike it, these are top-level installer entry
// points, not `defaults/` payload.
const SCRIPTS_INSTALL_LOOM: &str = include_str!("../../../scripts/install-loom.sh");
const INSTALL_SH: &str = include_str!("../../../install.sh");

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

/// The Rust class as the `git add -A` pathspec exclusion mirrors spell it
/// (#9134): trailing `/` stripped, then a trailing `*` appended so the
/// pathspec also excludes a sibling rename/backup of the credential path, not
/// just the exact path — matching [`super::is_credential_path`]'s widened
/// prefix-match contract (verified empirically to widen coverage the same
/// way for `git add -A -- . ':!...'` pathspecs, since `*` there is glob magic
/// that matches across `/`, unlike a bare literal pathspec element).
fn rust_class_pathspec_glob() -> BTreeSet<String> {
    CREDENTIAL_PATTERNS
        .iter()
        .map(|p| format!("{}*", p.trim_end_matches('/')))
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

/// Extract every `':!<path>'` exclusion from a `git add -A -- . ...` line.
/// Shared by the executed `git add -A -- .` calls in
/// scripts/install-loom.sh / install.sh.
fn pathspec_excludes(content: &str, source_name: &str) -> BTreeSet<String> {
    let line = content
        .lines()
        .find(|l| l.contains("git add -A -- ."))
        .unwrap_or_else(|| panic!("{source_name} must contain a `git add -A -- . ':!...'` line"));
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
fn install_loom_sh_pathspec_matches_rust_class() {
    // #8734: scripts/install-loom.sh's "Create initial commit" `git add -A`
    // (the non-git-repo bootstrap path) carries its own literal copy of the
    // exclusion pathspec — an `-A` add over a tree that is not yet a repo, so
    // there is no "what this run wrote" allowlist available to it the way
    // resync-installed.sh has one.
    assert_eq!(
        pathspec_excludes(SCRIPTS_INSTALL_LOOM, "scripts/install-loom.sh"),
        rust_class_pathspec_glob(),
        "scripts/install-loom.sh's initial-commit `git add -A` exclusions have drifted from \
         loom-daemon/src/init/post_init.rs CREDENTIAL_PATTERNS — keep them identical (#8005/#8734)"
    );
}

#[test]
fn install_sh_pathspec_matches_rust_class() {
    // #8734: install.sh's "Create initial commit" block is a separate literal
    // copy of the same pattern (same reachability as install-loom.sh's).
    assert_eq!(
        pathspec_excludes(INSTALL_SH, "install.sh"),
        rust_class_pathspec_glob(),
        "install.sh's initial-commit `git add -A` exclusions have drifted from \
         loom-daemon/src/init/post_init.rs CREDENTIAL_PATTERNS — keep them identical (#8005/#8734)"
    );
}

#[test]
fn is_credential_path_matching_contract() {
    // Directory patterns: the dir itself and anything under it.
    assert!(is_credential_path(".loom/tokens"));
    assert!(is_credential_path(".loom/tokens/acct-1.json"));
    assert!(is_credential_path(".loom/gh-config-by-owner/some-owner/hosts.yml"));
    assert!(is_credential_path(".loom/claude-config/builder-1/.credentials.json"));
    // File pattern: exact match.
    assert!(is_credential_path(".loom/accounts.env"));
    assert!(!is_credential_path(".loom/accounts.json"));
    // #9134: a sibling-renamed credential dir (e.g. `mv .loom/tokens
    // .loom/tokens.disabled-<ts>`, a natural operator move the repo's own
    // .gitignore documents the `name.bak-$(date +%Y%m%dT%H%M%SZ)` convention
    // for) must not escape the class just because it is no longer the exact
    // directory name. One example per credential-bearing directory:
    assert!(is_credential_path(".loom/tokens.disabled-20260101T000000Z/a.token"));
    assert!(is_credential_path(".loom/tokens-old/a.token"));
    assert!(is_credential_path(".loom/claude-config.bak-20260101T000000Z/builder-1/x"));
    assert!(is_credential_path(".loom/api-keys.disabled-20260101T000000Z/zai/x.env"));
    assert!(is_credential_path(".loom/gh-config.bak-20260101T000000Z/hosts.yml"));
    assert!(is_credential_path(
        ".loom/gh-config-by-owner.disabled-20260101T000000Z/some-owner/hosts.yml"
    ));
    // Same widening applies to the one file pattern, per decision (b) in
    // #9134: a hand-made backup of the account source file must also stay
    // covered.
    assert!(is_credential_path(".loom/accounts.env.bak"));
    // The contract is a uniform prefix match (decision (a) in #9134): a path
    // that merely LOOKS like a sibling by sharing the same string prefix is
    // deliberately swept in too, trading a few over-inclusive false
    // positives for no false negatives — see CREDENTIAL_PATTERNS' doc comment.
    assert!(is_credential_path(".loom/tokens-archive/x"));
    assert!(is_credential_path(".loom/gh-configx"));
    assert!(is_credential_path(".loom/accounts.env.example"));
    // Deliberately outside the class (see CREDENTIAL_PATTERNS docs) — none of
    // these share a credential pattern's prefix.
    assert!(!is_credential_path(".loom/account-health.json"));
    assert!(!is_credential_path(".loom-local/local.json"));
    assert!(!is_credential_path(".loom/hooks/foo.sh"));
}

/// The #9046 leak's literal footprint, path by path (#9141).
///
/// #9134 widened the *matching contract* to a prefix match, which is what
/// already covers these — but it was written before commit `a9da48c2` was
/// analysed, so nothing in the suite named the paths that actually leaked.
/// This pins them by name: every one of the 26 paths that reached a resync
/// commit is in the class, so no future "simplification" of the contract back
/// to exact-path matching can pass CI. `.loom/tokens` alone does NOT cover
/// them — that is precisely the assumption the incident falsified.
#[test]
fn the_a9da48c2_token_pool_copy_footprint_is_in_the_class() {
    let pool = ".loom/tokens.shadow-disabled-20260926T021559Z";
    // The 21 `.token` files.
    for i in 1..=21 {
        let p = format!("{pool}/acct-{i}.token");
        assert!(
            is_credential_path(&p),
            "{p} leaked in a9da48c2 and must be in the credential class (#9141)"
        );
    }
    // The pool's bookkeeping files, which leaked alongside them.
    for leaf in [
        ".ranking",
        ".ranking.classes.json",
        ".rotation_cursor",
        ".bad_tokens",
    ] {
        let p = format!("{pool}/{leaf}");
        assert!(
            is_credential_path(&p),
            "{p} leaked in a9da48c2 and must be in the credential class (#9141)"
        );
    }
    // The directory itself, and the `-` separator variant of the same rename.
    assert!(is_credential_path(pool));
    assert!(is_credential_path(".loom/tokens-shadow-disabled-20260926T021559Z/acct-1.token"));
}
