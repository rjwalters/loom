//! Issue #8997 gap (d): the `loom:building` label flip and the lease comment
//! report rate-limited failures to the breaker — each arm separately, once
//! per trip, with the workspace's own root/`gh` for the probe, and never for
//! ordinary failures. Breakers are injected via
//! [`SweepRegistryConfig::rate_limit`]; the global singleton is untouched.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::rate_limit_breaker::report::{
    BreakerHandle, BudgetProbe, FailureContext, ForgeProbe, ProbeMode,
};
use crate::rate_limit_breaker::{BudgetSnapshot, RateLimitBreakerConfig, SharedRateLimitBreaker};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::tempdir;

pub(in crate::sweep_registry) const RATE_LIMITED: &str =
    "GraphQL: API rate limit already exceeded for installation ID 151241294";

#[derive(Default)]
struct CountingProbe {
    calls: AtomicUsize,
    seen: Mutex<Vec<FailureContext>>,
}

impl BudgetProbe for CountingProbe {
    fn probe(&self, ctx: &FailureContext, _now: chrono::DateTime<Utc>) -> Option<BudgetSnapshot> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(ctx.clone());
        None
    }
}

fn breaker(enabled: bool) -> Arc<SharedRateLimitBreaker> {
    Arc::new(SharedRateLimitBreaker::new(RateLimitBreakerConfig {
        enabled,
        fallback_cooldown_secs: 900,
    }))
}

/// A fake `gh` whose writes fail with `stderr` (rate limit / 401) and whose
/// `rate_limit` probe calls report an exhausted core bucket resetting at
/// `reset`. Logs `$PWD|argv` per call.
pub(in crate::sweep_registry) fn failing_gh(
    ws: &Path,
    stderr: &str,
    reset: i64,
) -> (PathBuf, PathBuf) {
    let log = ws.join("gh.log");
    let gh = ws.join("fake-gh.sh");
    let body = format!(
        "#!/usr/bin/env bash\nprintf '%s|%s\\n' \"$PWD\" \"$*\" >> '{log}'\n\
         case \"$*\" in\n  'api rate_limit') printf '{{\"resources\":{{\"core\":{{\"remaining\":0,\"used\":15000,\"reset\":{reset}}}}}}}' ;;\n  \
         'api -i graphql'*) printf 'HTTP/2.0 200 OK\\r\\n\\r\\n{{\"data\":{{\"rateLimit\":{{\"used\":3,\"remaining\":4997,\"resetAt\":\"2026-07-29T19:20:35Z\"}}}}}}' ;;\n  \
         *) echo '{err}' >&2; exit 1 ;;\nesac\n",
        log = log.display(),
        err = stderr.replace('\'', ""),
    );
    std::fs::write(&gh, body).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    (gh, log)
}

fn registry(ws: &Path, gh: PathBuf, rate_limit: Option<BreakerHandle>) -> SweepRegistry {
    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.gh_bin = Some(gh);
    config.skip_label_flip = false;
    config.rate_limit = rate_limit;
    SweepRegistry::new(config)
}

fn handle(b: &Arc<SharedRateLimitBreaker>, probe: Arc<dyn BudgetProbe>) -> Option<BreakerHandle> {
    Some(BreakerHandle {
        breaker: Arc::clone(b),
        probe,
        mode: ProbeMode::Inline,
    })
}

fn lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

fn probe_calls(log: &Path) -> usize {
    lines(log)
        .iter()
        .filter(|l| l.contains("|api rate_limit") || l.contains("|api -i graphql"))
        .count()
}

#[test]
fn rate_limited_label_flip_trips_once_and_keeps_its_error() {
    let dir = tempdir().unwrap();
    let (gh, _log) = failing_gh(dir.path(), RATE_LIMITED, 0);
    let b = breaker(true);
    let probe = Arc::new(CountingProbe::default());
    let reg = registry(dir.path(), gh.clone(), handle(&b, probe.clone()));

    let err = reg.flip_label_to_building(8997).unwrap_err();
    assert!(err.to_string().contains("rate limit"), "flip still reports its failure: {err}");
    assert!(b.is_suppressed(Utc::now()));
    let snap = b.snapshot(Utc::now());
    assert_eq!(snap.source.as_deref(), Some("sweep_dispatch (label flip)"));
    // Cooldown repeats do not probe again.
    assert!(reg.flip_label_to_building(8997).is_err());
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    assert_eq!(b.snapshot(Utc::now()).trips_total, 1);
    let seen = probe.seen.lock().unwrap();
    assert_eq!(seen[0].root.as_deref(), Some(dir.path()), "probe in the workspace root");
    assert_eq!(seen[0].program.as_deref(), Some(gh.to_string_lossy().as_ref()));
}

#[test]
fn rate_limited_lease_comment_trips_independently() {
    let dir = tempdir().unwrap();
    let (gh, _log) = failing_gh(dir.path(), RATE_LIMITED, 0);
    let b = breaker(true);
    let probe = Arc::new(CountingProbe::default());
    let reg = registry(dir.path(), gh, handle(&b, probe.clone()));

    reg.write_lease_comment(8997, "sweep-issue-8997-1"); // still fail-open
    assert!(b.is_suppressed(Utc::now()));
    assert_eq!(b.snapshot(Utc::now()).source.as_deref(), Some("sweep_dispatch (lease comment)"));
    reg.write_lease_comment(8997, "sweep-issue-8997-1");
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn ordinary_write_failures_do_not_trip() {
    let dir = tempdir().unwrap();
    let (gh, log) = failing_gh(dir.path(), "HTTP 401: Bad credentials", 0);
    let b = breaker(true);
    let probe = Arc::new(CountingProbe::default());
    let reg = registry(dir.path(), gh, handle(&b, probe.clone()));

    assert!(reg.flip_label_to_building(8997).is_err());
    reg.write_lease_comment(8997, "sweep-issue-8997-1");
    assert!(!b.is_suppressed(Utc::now()));
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    assert_eq!(probe_calls(&log), 0, "no budget probe for an ordinary failure");
}

#[test]
fn disabled_or_unregistered_breaker_preserves_previous_behavior() {
    let dir = tempdir().unwrap();
    let (gh, log) = failing_gh(dir.path(), RATE_LIMITED, 0);
    let b = breaker(false);
    let probe = Arc::new(CountingProbe::default());
    let disabled = registry(dir.path(), gh.clone(), handle(&b, probe.clone()));
    let err = disabled.flip_label_to_building(8997).unwrap_err();
    assert!(err.to_string().contains("gh issue edit failed for #8997"));
    disabled.write_lease_comment(8997, "s");
    assert!(!b.is_suppressed(Utc::now()));
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    // No injected handle and no process-global breaker (tests never
    // register one): the arms behave exactly as before #8997.
    let unregistered = registry(dir.path(), gh, None);
    assert!(unregistered.flip_label_to_building(8997).is_err());
    unregistered.write_lease_comment(8997, "s");
    assert_eq!(probe_calls(&log), 0, "no budget probe without an enabled breaker");
}

#[test]
fn real_probe_reads_the_workspace_credential_reset() {
    let dir = tempdir().unwrap();
    let reset = Utc::now().timestamp() + 1200;
    let (gh, log) = failing_gh(dir.path(), RATE_LIMITED, reset);
    let b = breaker(true);
    let reg = registry(dir.path(), gh, handle(&b, Arc::new(ForgeProbe)));

    assert!(reg.flip_label_to_building(8997).is_err());
    let until = b.snapshot(Utc::now()).cooldown_until.unwrap();
    assert_eq!(until.timestamp(), reset, "core reset from the probe, not the 900s fallback");
    assert_eq!(probe_calls(&log), 2, "one bounded probe = two calls");
    let root = dir.path().canonicalize().unwrap();
    for call in lines(&log).iter().filter(|l| l.contains("|api ")) {
        let pwd = Path::new(call.split('|').next().unwrap())
            .canonicalize()
            .unwrap();
        assert_eq!(pwd, root, "probe ran in the failing call's workspace: {call}");
    }
}
