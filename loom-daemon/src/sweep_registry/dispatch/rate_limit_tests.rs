//! Issue #8997 gap (d) at the dispatch level: a rate-limited `loom:building`
//! flip trips the injected breaker on the first failure, while the current
//! attempt keeps its pre-#8997 semantics (dispatch continues, no lease
//! comment for a claim that was never flipped).
//!
//! Sibling file (declared from `dispatch.rs`) because `dispatch/tests.rs` is
//! over the file-size ratchet threshold.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::rate_limit_breaker::report::{BreakerHandle, BudgetProbe, FailureContext, ProbeMode};
use crate::rate_limit_breaker::{BudgetSnapshot, RateLimitBreakerConfig, SharedRateLimitBreaker};
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::tempdir;

#[derive(Default)]
struct CountingProbe(AtomicUsize);

impl BudgetProbe for CountingProbe {
    fn probe(&self, _ctx: &FailureContext, _now: DateTime<Utc>) -> Option<BudgetSnapshot> {
        self.0.fetch_add(1, Ordering::SeqCst);
        None
    }
}

#[test]
#[serial]
fn rate_limited_label_flip_trips_the_breaker_and_dispatch_continues() {
    let dir = tempdir().unwrap();
    let (registry, gh_log, spawn_log, comments_store) =
        lease_order_dispatch_registry(dir.path(), &[]);
    // Wrap the fixture's fake `gh`: only the label flip hits the limit.
    let inner = registry.config().gh_bin.clone().unwrap();
    let wrapper = dir.path().join("rate-limited-gh.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/usr/bin/env bash\nif [[ \"$1 $2\" == 'issue edit' ]]; then\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             echo 'GraphQL: API rate limit already exceeded for installation ID 1' >&2; exit 1\nfi\n\
             exec '{inner}' \"$@\"\n",
            log = gh_log.display(),
            inner = inner.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let breaker = Arc::new(SharedRateLimitBreaker::new(RateLimitBreakerConfig {
        enabled: true,
        fallback_cooldown_secs: 900,
    }));
    let probe = Arc::new(CountingProbe::default());
    let mut config = registry.config().clone();
    config.gh_bin = Some(wrapper);
    config.rate_limit = Some(BreakerHandle {
        breaker: Arc::clone(&breaker),
        probe: probe.clone(),
        mode: ProbeMode::Inline,
    });
    let mut registry = SweepRegistry::new(config);

    let outcome = registry.dispatch(&SweepKind::Issue(8997), None, None, None, None);
    assert!(outcome.is_ok(), "a failed flip still continues dispatch: {outcome:?}");

    assert!(breaker.is_suppressed(Utc::now()), "first rate-limited flip trips");
    assert_eq!(
        breaker.snapshot(Utc::now()).source.as_deref(),
        Some("sweep_dispatch (label flip)")
    );
    assert_eq!(probe.0.load(Ordering::SeqCst), 1);
    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert_eq!(gh_calls.matches("issue edit").count(), 1, "{gh_calls}");
    let stored = std::fs::read_to_string(&comments_store).unwrap_or_default();
    assert!(
        !stored.contains("loom:lease host="),
        "no lease for an unflipped claim: {stored}"
    );
    assert!(
        wait_for_contents(&spawn_log, "spawned", 5000),
        "current attempt's semantics preserved: the builder still spawns"
    );
}
