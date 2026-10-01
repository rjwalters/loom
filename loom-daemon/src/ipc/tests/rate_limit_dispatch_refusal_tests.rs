//! Issue #4666: `Request::DispatchSweep` previously consulted only the
//! host-distress breaker (`host_breaker`), never the GitHub rate-limit
//! breaker (`rate_limit_breaker`, #4429/#4440) — so a brand-new dispatch
//! could still land while the shared forge API budget was in a known
//! cooldown. These tests exercise [`rate_limit_dispatch_refusal`] — the
//! exact decision `handle_request`'s `DispatchSweep` arm makes — directly
//! with a manually constructed [`crate::rate_limit_breaker::RateLimitSnapshot`]
//! rather than through the process-global breaker: see that function's
//! doc comment for why (registering the real global would permanently
//! poison every other `DispatchSweep` test sharing this test binary under
//! plain `cargo test --workspace`, which this repo's CI still runs
//! alongside `cargo nextest run`).

use super::*;

fn suppressed_snapshot(
    cooldown_until: Option<chrono::DateTime<Utc>>,
) -> crate::rate_limit_breaker::RateLimitSnapshot {
    crate::rate_limit_breaker::RateLimitSnapshot {
        enabled: true,
        phase: crate::rate_limit_breaker::BreakerPhase::Cooldown,
        suppressed: true,
        source: Some("test_source".to_string()),
        tripped_at: Some(Utc::now()),
        cooldown_until,
        trips_total: 1,
        core_remaining: None,
        graphql_remaining: None,
        core_used: None,
        graphql_used: None,
        budget_probed_at: None,
    }
}

/// No breaker registered at all (`global_snapshot()` returns `None`)
/// must be a complete no-op — zero behavior change for daemons that
/// never enabled the breaker.
#[test]
fn no_snapshot_never_refuses() {
    let kind = SweepKind::Issue(4666);
    assert!(rate_limit_dispatch_refusal(&kind, None, false).is_none());
    assert!(rate_limit_dispatch_refusal(&kind, None, true).is_none());
}

/// A registered breaker that is Closed (not suppressed) must not
/// refuse either.
#[test]
fn closed_breaker_never_refuses() {
    let kind = SweepKind::Issue(4666);
    let snap = crate::rate_limit_breaker::RateLimitSnapshot {
        enabled: true,
        phase: crate::rate_limit_breaker::BreakerPhase::Closed,
        suppressed: false,
        source: None,
        tripped_at: None,
        cooldown_until: None,
        trips_total: 0,
        core_remaining: None,
        graphql_remaining: None,
        core_used: None,
        graphql_used: None,
        budget_probed_at: None,
    };
    assert!(rate_limit_dispatch_refusal(&kind, Some(&snap), false).is_none());
}

/// The core #4666 fix: a suppressed (Cooldown) snapshot refuses the
/// dispatch by default, with a message that (a) names the rate-limit
/// breaker and its cooldown release time, and (b) does not reuse the
/// host-distress breaker's wording — the two must never be conflated
/// since they have different root causes and different remediations.
#[test]
fn suppressed_breaker_refuses_with_distinct_message() {
    let kind = SweepKind::Issue(4666);
    let until = Utc::now() + chrono::Duration::seconds(600);
    let snap = suppressed_snapshot(Some(until));

    let response = rate_limit_dispatch_refusal(&kind, Some(&snap), false);
    match response {
        Some(Response::Error { message }) => {
            assert!(
                message.contains("rate-limit"),
                "expected the rate-limit breaker refusal message, got: {message}"
            );
            assert!(
                message.contains(&until.to_string()),
                "expected the cooldown release time in the message, got: {message}"
            );
            assert!(
                !message.contains("host circuit breaker") && !message.contains("host distress"),
                "rate-limit refusal must not be conflated with the host-distress \
                     breaker's wording: {message}"
            );
        }
        other => panic!("Expected Some(Response::Error), got: {other:?}"),
    }
}

/// A suppressed snapshot with no probed cooldown time yet must still
/// refuse, with an informative (not panicking/empty) fallback phrase.
#[test]
fn suppressed_breaker_without_cooldown_time_still_refuses() {
    let kind = SweepKind::Issue(4666);
    let snap = suppressed_snapshot(None);
    let response = rate_limit_dispatch_refusal(&kind, Some(&snap), false);
    assert!(
        matches!(response, Some(Response::Error { .. })),
        "expected a refusal even without a known cooldown release time, got: {response:?}"
    );
}

/// `force: true` overrides the rate-limit breaker independently of
/// the host-distress breaker's own `force` handling, even while the
/// snapshot itself remains suppressed throughout.
#[test]
fn force_true_overrides_suppressed_breaker() {
    let kind = SweepKind::Issue(4666);
    let snap = suppressed_snapshot(Some(Utc::now() + chrono::Duration::seconds(600)));
    assert!(
        rate_limit_dispatch_refusal(&kind, Some(&snap), true).is_none(),
        "force: true must bypass the rate-limit breaker refusal"
    );
}
