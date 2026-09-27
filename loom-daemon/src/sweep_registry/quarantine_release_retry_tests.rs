//! Quarantine-*release* retry bounding (Issue #8953): the retry ceiling that
//! stops a permanently-failing `loom:blocked` -> `loom:issue` label edit from
//! re-firing on every 30s reaper tick forever (the `#127` case — 68 retries
//! over ~34 minutes), and the shared-rate-limit-breaker check that skips —
//! without counting — while the shared GitHub API quota is exhausted.
//!
//! Lives in its own sibling module rather than `quarantine.rs`'s `mod tests`:
//! that file is already over the file-size ratchet's threshold and therefore
//! frozen at its current size (see `.loom/docs/file-size-policy.md`), and this
//! mirrors the existing `quarantine_empty_pool_tests.rs` /
//! `quarantine_dispatch_scope_tests.rs` precedent.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{QuarantineConfig, SweepRegistry, SweepRegistryConfig};
use crate::sweep_registry::test_support::install_fake_gh;
use tempfile::tempdir;

/// AC: `attempt_quarantine_release` stops retrying an issue once its
/// consecutive-failure tally reaches `QuarantineConfig::max_release_attempts`
/// — the `#127` case this issue is named for (68 retries over ~34 minutes
/// with no ceiling at all). Once the ceiling is hit the issue leaves BOTH
/// `pending_quarantine_release` and the attempt tally, so no further tick
/// makes any further `gh` call for it.
#[test]
fn attempt_quarantine_release_stops_retrying_after_ceiling() {
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh-invocations.log");
    let fake_gh = install_fake_gh(dir.path(), &gh_log, "", 1); // always fails

    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    let mut registry = SweepRegistry::new(config);
    registry.set_quarantine_config(QuarantineConfig {
        max_release_attempts: 3,
        ..QuarantineConfig::default()
    });
    registry.pending_quarantine_release.insert(127);

    registry.retry_pending_quarantine_releases();
    assert!(registry.pending_quarantine_release_issues().contains(&127));
    assert_eq!(registry.quarantine_release_attempts.get(&127).copied(), Some(1));

    registry.retry_pending_quarantine_releases();
    assert!(registry.pending_quarantine_release_issues().contains(&127));
    assert_eq!(registry.quarantine_release_attempts.get(&127).copied(), Some(2));

    // Third failure hits the ceiling (3): give up, clear both records.
    registry.retry_pending_quarantine_releases();
    assert!(
        !registry.pending_quarantine_release_issues().contains(&127),
        "issue must stop being retried once the ceiling is reached"
    );
    assert!(!registry.quarantine_release_attempts.contains_key(&127));

    // A further tick makes no additional gh call at all: nothing pending.
    let calls_before = std::fs::read_to_string(&gh_log).unwrap_or_default();
    registry.retry_pending_quarantine_releases();
    let calls_after = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert_eq!(
        calls_before, calls_after,
        "no further gh calls once the ceiling has been hit and the issue dropped"
    );
}

/// AC: while the (injected, test-only) rate-limit breaker reports
/// suppressed, `retry_pending_quarantine_releases`' pass-level check skips
/// the ENTIRE pending set with zero `gh` calls and zero attempt-tally
/// consumption — a suppressed skip must never count against the ceiling,
/// so a genuinely stuck issue gets its full N attempts only against real
/// (non-suppressed) tries.
#[test]
fn retry_pending_quarantine_releases_skips_without_counting_while_rate_limited() {
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh-invocations.log");
    let fake_gh = install_fake_gh(dir.path(), &gh_log, "", 1); // would fail if ever called

    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    let mut registry = SweepRegistry::new(config);
    registry.pending_quarantine_release.insert(999);
    registry.test_force_rate_limited = true;

    for _ in 0..5 {
        registry.retry_pending_quarantine_releases();
    }
    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.is_empty(),
        "no gh call should be made while suppressed; got: {gh_calls:?}"
    );
    assert!(registry.pending_quarantine_release_issues().contains(&999));
    assert!(
        !registry.quarantine_release_attempts.contains_key(&999),
        "a suppressed skip must not consume any of the retry ceiling"
    );

    // Once suppression clears, the retry resumes normally (and DOES count).
    registry.test_force_rate_limited = false;
    registry.retry_pending_quarantine_releases();
    let gh_calls_after = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !gh_calls_after.is_empty(),
        "the retry should resume once suppression clears; got: {gh_calls_after:?}"
    );
    assert_eq!(registry.quarantine_release_attempts.get(&999).copied(), Some(1));
}

/// Same guarantee as above, but proves the backstop lives INSIDE
/// `attempt_quarantine_release` itself, not only in
/// `retry_pending_quarantine_releases`'s pass-level check — calls it
/// directly, mirroring `clear_quarantine`'s immediate, operator-driven
/// release attempt (the other production call site).
#[test]
fn attempt_quarantine_release_direct_call_skips_while_rate_limited() {
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh-invocations.log");
    let fake_gh = install_fake_gh(dir.path(), &gh_log, "", 1);

    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    let mut registry = SweepRegistry::new(config);
    registry.test_force_rate_limited = true;

    registry.attempt_quarantine_release(555);

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.is_empty(),
        "no gh call should be made while suppressed; got: {gh_calls:?}"
    );
    assert!(registry.pending_quarantine_release_issues().contains(&555));
    assert!(!registry.quarantine_release_attempts.contains_key(&555));
}
