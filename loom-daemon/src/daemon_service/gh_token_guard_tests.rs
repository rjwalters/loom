//! Tests for the #4456 value-changed guard on the `github-app` refresh
//! tick. Deliberately a *pure* function test — it does not touch the
//! process-global `GH_TOKEN`, so it neither races nor adds to the
//! test-env-mutation hazard tracked by #4385.
//!
//! A sibling file rather than an inline `mod` in `daemon_service.rs` (#9596):
//! that file is a frozen over-threshold ratchet entry, and extracting a test
//! module is the path `.loom/docs/file-size-policy.md` names for making room
//! in one. The tests below are unchanged from their inline form.

use super::gh_token_needs_update;

#[test]
fn cache_hit_same_value_skips_write() {
    // The ~10-of-11 common case: the shell helper returned the unchanged
    // cached token, so no `set_var` is warranted.
    assert!(!gh_token_needs_update(Some("ghs_abc"), "ghs_abc"));
}

#[test]
fn genuine_rotation_writes() {
    // A real ~hourly rotation: the minted value differs -> write.
    assert!(gh_token_needs_update(Some("ghs_old"), "ghs_new"));
}

#[test]
fn unset_env_writes() {
    // Nothing exported yet (Err(VarError) -> None): treat as different so
    // the first post-boot tick still exports the token.
    assert!(gh_token_needs_update(None, "ghs_first"));
}

#[test]
fn empty_current_differs_from_nonempty() {
    // An empty exported value is not equal to a real minted token.
    assert!(gh_token_needs_update(Some(""), "ghs_real"));
}
