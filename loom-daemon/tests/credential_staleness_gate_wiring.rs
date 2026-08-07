// Integration test for the stale-forge-credential gate hold (#5630).
//
// The unit tests in `credential_preflight` deliberately drive a *local*
// `CredentialStreaks` value and never touch the process-global tracker, because
// `main_health_gate`'s production `GlobalCredentialFreshness` reads that
// singleton — mutating it from a unit test would non-deterministically flip
// unrelated gate tests running concurrently in the same process into
// `ForgeCredentialStale`.
//
// That leaves exactly one thing unproven at unit level: that the daemon's
// refresh loops and the gate are wired to the *same* singleton. This file
// proves it in its own process, where there is nothing else to contaminate.
//
// expect/unwrap are acceptable here since tests should panic on failure.
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

use loom_daemon::credential_preflight::{
    credential_source_for_owner, forge_credential_stale, forge_credential_stale_summary,
    record_forge_credential_failure, record_forge_credential_success, CREDENTIAL_SOURCE_PRIMARY,
};
use loom_daemon::main_health_gate::{CredentialFreshness, GlobalCredentialFreshness};

/// A single test function: the tracker is process-global, so splitting this
/// into several `#[test]`s would make them race each other inside this binary.
#[test]
fn refresh_loop_failures_are_visible_to_the_main_health_gate() {
    let gate_view = GlobalCredentialFreshness;

    assert!(!forge_credential_stale(), "a fresh process has no failing credential source");
    assert!(!gate_view.is_stale(), "and the gate agrees — it reads the same tracker");
    assert!(forge_credential_stale_summary().is_none());

    // The primary (#4430) refresh tick times out, exactly as observed on the
    // saturated host that motivated #5630.
    record_forge_credential_failure(
        CREDENTIAL_SOURCE_PRIMARY,
        "could not run github-app-token.sh get-token after 2 attempt(s): `bash` timed out after 90s",
    );
    assert!(
        gate_view.is_stale(),
        "the gate must see the refresh tick's failure — this is the whole wiring under test"
    );
    let summary = forge_credential_stale_summary().expect("a stale window has a summary");
    assert!(
        summary.contains(CREDENTIAL_SOURCE_PRIMARY),
        "the operator-facing summary must name the failing source: {summary}"
    );
    assert!(
        !summary.contains("ghs_"),
        "the summary must never carry a token-shaped value: {summary}"
    );

    // A cross-owner (#5401) loop failing too is a *separate* source: the
    // primary recovering must not clear it.
    let owner = credential_source_for_owner("2AMLogic/2am");
    record_forge_credential_failure(&owner, "`bash` timed out after 90s");
    record_forge_credential_success(CREDENTIAL_SOURCE_PRIMARY);
    assert!(
        gate_view.is_stale(),
        "the primary's recovery must not clear a still-failing per-owner credential"
    );
    assert!(
        forge_credential_stale_summary()
            .expect("still stale")
            .contains("2AMLogic/2am"),
        "the surviving streak is the one now described"
    );

    // Once every source recovers, the gate goes straight back to evaluating —
    // the hold is not sticky (the anti-oscillation property, AC3).
    record_forge_credential_success(&owner);
    assert!(
        !gate_view.is_stale(),
        "with every source healthy the gate evaluates normally again on the very next tick"
    );
    assert!(forge_credential_stale_summary().is_none());
}
