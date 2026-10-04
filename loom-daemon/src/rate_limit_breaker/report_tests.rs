//! Issue #8997: contextual reset evidence, false-full readings, and the
//! trip-first / no-storm / no-recursion reporting contract. Every test
//! injects its own breaker and probe — the process-global singleton is never
//! registered.

#![allow(clippy::unwrap_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use super::evidence::{self, ResetEvidence};
use super::report::{
    report_failure, BreakerHandle, BudgetProbe, FailureContext, ProbeMode, Reported,
};
use super::{BudgetSnapshot, RateLimitBreakerConfig, SharedRateLimitBreaker, TransitionKind};

const EPOCH: i64 = 1_785_352_000;
const PRIMARY_GQL: &str = "GraphQL: API rate limit already exceeded for user ID 1";
const PRIMARY_REST: &str = "HTTP 403: API rate limit exceeded for installation ID 151241294";

fn t(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(EPOCH + secs, 0).unwrap()
}

fn breaker(enabled: bool) -> Arc<SharedRateLimitBreaker> {
    Arc::new(SharedRateLimitBreaker::new(RateLimitBreakerConfig {
        enabled,
        fallback_cooldown_secs: 900,
    }))
}

fn snapshot(core: (u64, i64), graphql: (u64, i64)) -> BudgetSnapshot {
    BudgetSnapshot {
        core_remaining: core.0,
        core_reset: t(core.1),
        graphql_remaining: graphql.0,
        graphql_reset: t(graphql.1),
        core_used: Some(5000 - core.0.min(5000)),
        graphql_used: Some(5000 - graphql.0.min(5000)),
        probed_at: t(0),
    }
}

const HEALTHY: ((u64, i64), (u64, i64)) = ((5000, 3600), (5000, 3600));

/// A probe that answers per credential: the installation root (or explicit
/// installation config dir) reads `installation`; anything else — the
/// ambient user credential — reads `ambient`. Counts calls and records the
/// contexts it saw.
struct FixtureProbe {
    installation_root: PathBuf,
    installation: Option<BudgetSnapshot>,
    ambient: Option<BudgetSnapshot>,
    calls: AtomicUsize,
    seen: Mutex<Vec<FailureContext>>,
    delay: std::time::Duration,
}

impl FixtureProbe {
    fn new(installation: Option<BudgetSnapshot>, ambient: Option<BudgetSnapshot>) -> Arc<Self> {
        Arc::new(Self {
            installation_root: PathBuf::from("/ws/installation-repo"),
            installation,
            ambient,
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            delay: std::time::Duration::ZERO,
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl BudgetProbe for FixtureProbe {
    fn probe(&self, ctx: &FailureContext, _now: DateTime<Utc>) -> Option<BudgetSnapshot> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(ctx.clone());
        std::thread::sleep(self.delay);
        if ctx.root.as_deref() == Some(self.installation_root.as_path()) {
            self.installation
        } else {
            self.ambient
        }
    }
}

fn handle(
    b: &Arc<SharedRateLimitBreaker>,
    probe: Arc<dyn BudgetProbe>,
    mode: ProbeMode,
) -> BreakerHandle {
    BreakerHandle {
        breaker: Arc::clone(b),
        probe,
        mode,
    }
}

fn installation_ctx() -> FailureContext {
    FailureContext::for_root("/ws/installation-repo", "/fake/gh")
}

fn until_of(r: Option<Reported>) -> DateTime<Utc> {
    r.unwrap().transition.until.unwrap()
}

// ===== (b) the failing credential's reset wins over a healthy ambient one =====

#[test]
fn installation_core_reset_selected_over_healthy_ambient_user() {
    let b = breaker(true);
    let probe = FixtureProbe::new(
        Some(snapshot((0, 1200), (4000, 3000))),
        Some(snapshot(HEALTHY.0, HEALTHY.1)),
    );
    let h = handle(&b, probe.clone(), ProbeMode::Inline);
    let r = report_failure(&h, PRIMARY_REST, "label flip", installation_ctx(), t(0));
    assert_eq!(until_of(r), t(1200), "core reset of the failing installation");
    assert_eq!(probe.calls(), 1);
    let seen = probe.seen.lock().unwrap();
    assert_eq!(seen[0].root.as_deref(), Some(std::path::Path::new("/ws/installation-repo")));
    assert_eq!(
        seen[0].program.as_deref(),
        Some("/fake/gh"),
        "probe keeps the failing call's program"
    );
    assert_eq!(b.snapshot(t(1)).core_remaining, Some(0));
}

#[test]
fn installation_graphql_reset_selected_over_healthy_ambient_user() {
    let b = breaker(true);
    let probe = FixtureProbe::new(
        Some(snapshot((4000, 3000), (0, 1500))),
        Some(snapshot(HEALTHY.0, HEALTHY.1)),
    );
    let h = handle(&b, probe.clone(), ProbeMode::Inline);
    let r = report_failure(&h, PRIMARY_GQL, "safehouse", installation_ctx(), t(0));
    assert_eq!(until_of(r), t(1500));
    assert_eq!(b.snapshot(t(1)).graphql_remaining, Some(0));
}

#[test]
fn healthy_ambient_reading_cannot_pass_as_healthy_quota() {
    // No context → the probe reads the ambient user's healthy budget. That
    // contradicts a primary failure: fallback cooldown, and status must not
    // cache the healthy numbers.
    let b = breaker(true);
    let probe = FixtureProbe::new(None, Some(snapshot(HEALTHY.0, HEALTHY.1)));
    let h = handle(&b, probe.clone(), ProbeMode::Inline);
    let r = report_failure(&h, PRIMARY_GQL, "work_finder", FailureContext::default(), t(0));
    assert_eq!(until_of(r), t(900));
    let snap = b.snapshot(t(1));
    assert!(snap.suppressed);
    assert_eq!(snap.core_remaining, None, "untrusted reading not shown as the budget");
    assert_eq!(snap.graphql_remaining, None);
}

// ===== false-full /rate_limit vs authoritative failure headers =====

const EXHAUSTED_CORE_HEAD: &str = "HTTP/2.0 403 Forbidden\r\nX-Ratelimit-Limit: 15000\r\nX-Ratelimit-Remaining: 0\r\nX-Ratelimit-Reset: 1785353500\r\nX-Ratelimit-Used: 15000\r\nX-Ratelimit-Resource: core\r\n\r\n{\"message\":\"API rate limit exceeded\"}";

#[test]
fn failure_headers_beat_contradictory_full_rate_limit_body() {
    let b = breaker(true);
    // The probe would report a completely untouched budget (the new-
    // installation false-full case) — it must not even be consulted.
    let probe = FixtureProbe::new(
        Some(snapshot(HEALTHY.0, HEALTHY.1)),
        Some(snapshot(HEALTHY.0, HEALTHY.1)),
    );
    let h = handle(&b, probe.clone(), ProbeMode::Inline);
    let ctx = FailureContext {
        response_head: Some(EXHAUSTED_CORE_HEAD.to_owned()),
        ..installation_ctx()
    };
    let r = report_failure(&h, PRIMARY_REST, "ci_telemetry", ctx, t(0));
    assert_eq!(until_of(r), t(1500), "X-RateLimit-Reset of the failing response");
    assert_eq!(probe.calls(), 0, "authoritative headers need no probe");
}

#[test]
fn false_full_probe_without_headers_takes_the_fallback() {
    let b = breaker(true);
    let probe = FixtureProbe::new(Some(snapshot(HEALTHY.0, HEALTHY.1)), None);
    let h = handle(&b, probe, ProbeMode::Inline);
    let r = report_failure(&h, PRIMARY_REST, "claim_reconciliation", installation_ctx(), t(0));
    assert_eq!(until_of(r), t(900));
    assert_eq!(b.snapshot(t(1)).core_remaining, None);
}

#[test]
fn untrustworthy_failure_headers_fall_through_to_probe_or_fallback() {
    let reset = EPOCH + 1500;
    let cases = [
        String::new(),
        "HTTP/2.0 403 Forbidden\r\nX-Ratelimit-Remaining: zero\r\n".to_owned(),
        format!("X-Ratelimit-Remaining: 0\nX-Ratelimit-Reset: {reset}\n"), // no resource
        format!("X-Ratelimit-Remaining: 0\nX-Ratelimit-Reset: {reset}\nX-Ratelimit-Resource: search\n"),
        format!("X-Ratelimit-Remaining: 12\nX-Ratelimit-Reset: {reset}\nX-Ratelimit-Resource: core\n"),
        format!("X-Ratelimit-Remaining: 0\nX-Ratelimit-Reset: {}\nX-Ratelimit-Resource: core\n", EPOCH - 10),
        // A body line after the blank separator never counts as a header.
        format!("HTTP/2.0 403\n\nX-Ratelimit-Remaining: 0\nX-Ratelimit-Reset: {reset}\nX-Ratelimit-Resource: core\n"),
    ];
    for head in &cases {
        assert_eq!(evidence::from_failure_headers(head, t(0)), None, "head: {head:?}");
        let b = breaker(true);
        let probe = FixtureProbe::new(None, None);
        let h = handle(&b, probe.clone(), ProbeMode::Inline);
        let ctx = FailureContext {
            response_head: Some(head.clone()),
            ..installation_ctx()
        };
        let r = report_failure(&h, PRIMARY_REST, "x", ctx, t(0));
        assert_eq!(until_of(r), t(900), "configured fallback for head {head:?}");
        assert_eq!(probe.calls(), 1, "untrusted headers still get one contextual probe");
    }
}

#[test]
fn graphql_resource_headers_are_trusted() {
    let head = format!(
        "X-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {}\r\nX-RateLimit-Resource: graphql\r\n",
        EPOCH + 700
    );
    assert_eq!(
        evidence::from_failure_headers(&head, t(0)),
        Some(ResetEvidence::FailureResponse {
            resource: "graphql".to_owned(),
            reset: t(700)
        })
    );
}

#[test]
fn expired_and_secondary_readings_use_clamped_fallback() {
    // Exhausted bucket whose reset already passed: stale → fallback, hidden.
    let (ev, shown) =
        evidence::from_probe(PRIMARY_GQL, Some(&snapshot((0, -50), (5000, 3600))), t(0));
    assert!(!ev.is_trusted() && !shown);
    assert_eq!(ev.cooldown_until(t(0), 900), t(900));
    // Secondary limit: healthy primaries are expected — fallback, but shown.
    let (ev, shown) = evidence::from_probe(
        "You have exceeded a secondary rate limit.",
        Some(&snapshot(HEALTHY.0, HEALTHY.1)),
        t(0),
    );
    assert!(!ev.is_trusted() && shown);
    // Clamps hold for every source.
    assert_eq!(ev.cooldown_until(t(0), 5), t(super::MIN_COOLDOWN_SECS));
    assert_eq!(
        ResetEvidence::Probe { reset: t(90_000) }.cooldown_until(t(0), 900),
        t(super::MAX_COOLDOWN_SECS)
    );
    assert!(evidence::is_secondary_limit("You have triggered an abuse detection mechanism"));
    assert!(!evidence::is_secondary_limit(PRIMARY_GQL));
}

#[test]
fn a_new_trip_never_reuses_a_previous_trips_budget() {
    let b = breaker(true);
    b.observe_failure(PRIMARY_GQL, "a", Some(snapshot((5000, 0), (0, 300))), t(0))
        .unwrap();
    assert!(b.observe_tick(t(300)).is_some());
    // Second trip with no reading: the fallback, not a floor-length window
    // derived from the first trip's long-past reset.
    let tr = b.observe_failure(PRIMARY_GQL, "b", None, t(400)).unwrap();
    assert_eq!(tr.until, Some(t(1300)));
}

// ===== no storms, no recursion, ordinary failures untouched =====

#[test]
fn repeats_during_cooldown_never_probe_again() {
    let b = breaker(true);
    let probe = FixtureProbe::new(Some(snapshot((0, 1200), HEALTHY.1)), None);
    let h = handle(&b, probe.clone(), ProbeMode::Inline);
    assert!(report_failure(&h, PRIMARY_REST, "label flip", installation_ctx(), t(0)).is_some());
    for i in 1..20 {
        assert!(
            report_failure(&h, PRIMARY_REST, "lease comment", installation_ctx(), t(i)).is_none()
        );
    }
    assert_eq!(probe.calls(), 1);
    assert_eq!(b.snapshot(t(30)).trips_total, 1);
}

struct ReentrantProbe {
    handle: Mutex<Option<BreakerHandle>>,
    calls: AtomicUsize,
    nested: Mutex<Option<bool>>,
}

impl BudgetProbe for ReentrantProbe {
    fn probe(&self, ctx: &FailureContext, _now: DateTime<Utc>) -> Option<BudgetSnapshot> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // The probe's own gh call hits the limit and reports it (a moment
        // after the trip, on the tests' fixed clock).
        let h = self.handle.lock().unwrap().clone().unwrap();
        let nested = report_failure(&h, PRIMARY_REST, "probe", ctx.clone(), t(1));
        *self.nested.lock().unwrap() = Some(nested.is_some());
        None
    }
}

#[test]
fn a_report_from_inside_the_probe_cannot_recurse() {
    let b = breaker(true);
    let probe = Arc::new(ReentrantProbe {
        handle: Mutex::new(None),
        calls: AtomicUsize::new(0),
        nested: Mutex::new(None),
    });
    let h = handle(&b, probe.clone(), ProbeMode::Inline);
    *probe.handle.lock().unwrap() = Some(h.clone());
    let r = report_failure(&h, PRIMARY_REST, "label flip", installation_ctx(), t(0));
    assert!(r.is_some());
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1, "no second probe");
    assert_eq!(*probe.nested.lock().unwrap(), Some(false), "nested report absorbed");
}

#[test]
fn ordinary_failures_neither_trip_nor_probe() {
    let b = breaker(true);
    let probe = FixtureProbe::new(Some(snapshot((0, 1200), HEALTHY.1)), None);
    let h = handle(&b, probe.clone(), ProbeMode::Inline);
    for text in [
        "HTTP 401: Bad credentials",
        "gh: Not Found (HTTP 404)",
        "error connecting to api.github.com: dial tcp: i/o timeout",
        "could not run gh: No such file or directory",
        "timed out after 20s",
    ] {
        assert!(report_failure(&h, text, "x", installation_ctx(), t(0)).is_none());
    }
    assert_eq!(probe.calls(), 0);
    assert!(!b.is_suppressed(t(1)));
}

#[test]
fn disabled_breaker_reports_nothing() {
    let b = breaker(false);
    let probe = FixtureProbe::new(Some(snapshot((0, 1200), HEALTHY.1)), None);
    let h = handle(&b, probe.clone(), ProbeMode::Inline);
    assert!(report_failure(&h, PRIMARY_REST, "x", installation_ctx(), t(0)).is_none());
    assert_eq!(probe.calls(), 0);
    assert!(!b.is_suppressed(t(1)));
}

#[test]
fn background_mode_trips_immediately_and_refines_off_thread() {
    let b = breaker(true);
    let probe = Arc::new(FixtureProbe {
        delay: std::time::Duration::from_millis(400),
        ..Arc::try_unwrap(FixtureProbe::new(Some(snapshot((5000, 3600), (0, 1200))), None))
            .ok()
            .unwrap()
    });
    let h = handle(&b, probe.clone(), ProbeMode::Background);
    let started = std::time::Instant::now();
    let r = report_failure(&h, PRIMARY_GQL, "safehouse", installation_ctx(), t(0)).unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_millis(300),
        "caller never waits on the probe"
    );
    assert_eq!(r.transition.kind, TransitionKind::Tripped);
    assert_eq!(r.transition.until, Some(t(900)), "provisional fallback window");
    assert!(b.is_suppressed(t(1)), "suppression is immediate");
    r.refine.unwrap().join().unwrap();
    assert_eq!(b.snapshot(t(1)).cooldown_until, Some(t(1200)), "refined to the probed reset");
    assert_eq!(probe.calls(), 1);
}

// ===== the real forge probe keeps the failing call's credential =====

/// A fake `gh` that answers per `GH_CONFIG_DIR`: the installation's config
/// dir reads exhausted (core and graphql), anything else reads healthy.
fn fake_gh(dir: &std::path::Path, installation_cfg: &std::path::Path) -> PathBuf {
    let log = dir.join("gh.log");
    let script = dir.join("gh");
    let body = format!(
        r#"#!/usr/bin/env bash
printf '%s|%s\n' "${{GH_CONFIG_DIR:-}}" "$*" >> "{log}"
if [ "${{GH_CONFIG_DIR:-}}" = "{inst}" ]; then core=0; gql=0; else core=5000; gql=5000; fi
case "$*" in
  "api rate_limit") printf '{{"resources":{{"core":{{"limit":5000,"used":1,"remaining":%s,"reset":{reset}}}}}}}' "$core" ;;
  "api -i graphql"*) printf 'HTTP/2.0 200 OK\r\n\r\n{{"data":{{"rateLimit":{{"limit":5000,"used":9,"remaining":%s,"resetAt":"2026-07-29T19:20:35Z"}}}}}}' "$gql" ;;
  *) exit 1 ;;
esac
"#,
        log = log.display(),
        inst = installation_cfg.display(),
        reset = EPOCH + 1100,
    );
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[test]
fn forge_probe_reads_the_failing_credential_not_the_ambient_one() {
    let dir = tempfile::tempdir().unwrap();
    let inst = dir.path().join("gh-config-installation");
    std::fs::create_dir_all(&inst).unwrap();
    let gh = fake_gh(dir.path(), &inst);
    let now = t(0);

    let ctx = FailureContext {
        config_dir: Some(inst.clone()),
        ..FailureContext::for_root(dir.path(), gh.to_string_lossy())
    };
    let b = super::forge::probe_budget_ctx(&ctx, now).unwrap();
    assert_eq!((b.core_remaining, b.graphql_remaining), (0, 0));
    assert_eq!(b.core_reset, t(1100));

    let ambient = FailureContext {
        program: Some(gh.to_string_lossy().into_owned()),
        ..FailureContext::default()
    };
    let a = super::forge::probe_budget_ctx(&ambient, now).unwrap();
    assert_eq!((a.core_remaining, a.graphql_remaining), (5000, 5000));

    let log = std::fs::read_to_string(dir.path().join("gh.log")).unwrap();
    assert_eq!(log.lines().count(), 4, "two bounded calls per probe: {log}");
}
