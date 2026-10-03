//! The liveness loop skips its pass while the rate-limit breaker is tripped (#10020).

use crate::star_liveness::task::run_pass_unless_suppressed;

#[test]
fn a_suppressed_breaker_skips_the_pass_without_running_it() {
    let mut logged = false;
    let mut ran = false;
    let did = run_pass_unless_suppressed(|| true, &mut logged, || ran = true);
    assert!(!did);
    assert!(!ran, "no forge call or intent drain while suppressed");
    assert!(logged, "first skip is recorded so later skips log at debug");
}

#[test]
fn a_clear_breaker_runs_the_pass_and_resets_the_episode() {
    let mut logged = true;
    let mut ran = false;
    let did = run_pass_unless_suppressed(|| false, &mut logged, || ran = true);
    assert!(did);
    assert!(ran);
    assert!(!logged);
}
