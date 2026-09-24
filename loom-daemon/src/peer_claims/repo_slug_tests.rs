//! [`super::repo_slug`] coverage, kept out of `peer_claims.rs` for two
//! reasons: the test mutates the process-global `LOOM_REPO`, so it must carry
//! that variable's crate-wide `#[serial]` key (#8496 — see
//! `gh_repo_env.rs`'s test-isolation invariant), and `peer_claims.rs` is
//! frozen at its current size by `scripts/file-size-baseline.txt`, whose own
//! preferred remedy is a sibling module.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use serial_test::serial;

/// `$LOOM_REPO` wins when set; otherwise the workspace directory basename is
/// the (cross-host-stable) fallback key.
#[test]
#[serial]
fn repo_slug_prefers_env_then_basename() {
    // Env override wins.
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    assert_eq!(repo_slug(Path::new("/anything/here")), "rjwalters/loom");
    std::env::remove_var("LOOM_REPO");
    // Basename fallback is cross-host-stable for the same repo.
    assert_eq!(repo_slug(Path::new("/Users/a/loom")), "loom");
    assert_eq!(repo_slug(Path::new("/home/b/loom")), "loom");
}
