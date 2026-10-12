//! Issue #8997 gap (c): safehouse forge lookups consult and feed the shared
//! rate-limit breaker. Breakers are injected per test through
//! [`forge_gate::scoped`]; the process-global singleton is never registered.
//! Subprocess counts come from a fake `gh` that logs `$PWD|argv` per call.

#![allow(clippy::unwrap_used)]

use super::tests::{stub_server, SafehouseTestPaths};
use super::*;
use crate::rate_limit_breaker::evidence::ResetEvidence;
use crate::rate_limit_breaker::report::{BreakerHandle, BudgetProbe, FailureContext, ProbeMode};
use crate::rate_limit_breaker::{BudgetSnapshot, RateLimitBreakerConfig, SharedRateLimitBreaker};
use crate::types::{SweepId, SweepKind};
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::net::UnixListener;

const RATE_LIMITED: &str = "GraphQL: API rate limit already exceeded for installation ID 151241294";

/// Counts probes and records the context each ran under; reads nothing.
#[derive(Default)]
struct CountingProbe {
    calls: AtomicUsize,
    seen: std::sync::Mutex<Vec<FailureContext>>,
}

impl BudgetProbe for CountingProbe {
    fn probe(&self, ctx: &FailureContext, _now: DateTime<Utc>) -> Option<BudgetSnapshot> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(ctx.clone());
        None
    }
}

fn breaker() -> Arc<SharedRateLimitBreaker> {
    Arc::new(SharedRateLimitBreaker::new(RateLimitBreakerConfig {
        enabled: true,
        fallback_cooldown_secs: 60,
    }))
}

/// A breaker already cooling, releasing `window_secs` from now (tripped in
/// the past with the 60s fallback, so the release is real wall-clock time).
fn cooling(window_secs: i64) -> Arc<SharedRateLimitBreaker> {
    let b = breaker();
    let tripped = Utc::now() - chrono::Duration::seconds(60 - window_secs);
    b.trip("test", &ResetEvidence::Fallback { reason: "test" }, tripped)
        .unwrap();
    b
}

fn gate(b: &Arc<SharedRateLimitBreaker>, probe: &Arc<CountingProbe>) -> Option<BreakerHandle> {
    Some(BreakerHandle {
        breaker: Arc::clone(b),
        probe: Arc::clone(probe) as Arc<dyn BudgetProbe>,
        mode: ProbeMode::Inline,
    })
}

/// A fake `gh` serving every lookup the sink makes. A workspace containing
/// `.ratelimited` (or `.unauthorized`) fails every call with that stderr.
fn fake_gh(dir: &Path, pr_json: &str) -> (PathBuf, PathBuf) {
    let log = dir.join("gh-calls.log");
    let script = dir.join("fake-gh.sh");
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let body = format!(
        "#!/usr/bin/env bash\nprintf '%s|%s\\n' \"$PWD\" \"$*\" >> {log}\n\
         if [ -e .ratelimited ]; then echo {rl} >&2; exit 1; fi\n\
         if [ -e .unauthorized ]; then echo 'HTTP 401: Bad credentials' >&2; exit 1; fi\n\
         case \"$1 $2\" in\n  'pr list') printf '%s\\n' {pr} ;;\n  \
         'repo view') printf '%s\\n' {repo} ;;\n  'issue view') echo 'A title' ;;\n  \
         *) exit 1 ;;\nesac\n",
        log = quote(&log.to_string_lossy()),
        rl = quote(RATE_LIMITED),
        pr = quote(pr_json),
        repo = quote(r#"{"nameWithOwner":"rjwalters/loom","isPrivate":false}"#),
    );
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, log)
}

fn calls(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

fn merged_row(issue: u32) -> String {
    let merged_at = Utc::now() - chrono::Duration::minutes(10);
    format!(
        r#"[{{"number":{issue},"headRefName":"feature/issue-{issue}","url":"https://github.com/rjwalters/loom/pull/{issue}","mergedAt":"{}","createdAt":"{}","title":"t","additions":1,"deletions":1}}]"#,
        merged_at.to_rfc3339(),
        (merged_at - chrono::Duration::minutes(30)).to_rfc3339(),
    )
}

fn workspace(parent: &Path, name: &str) -> PathBuf {
    let ws = parent.join(name);
    std::fs::create_dir_all(&ws).unwrap();
    ws
}

#[tokio::test]
#[serial]
async fn cooling_breaker_makes_zero_forge_calls_for_every_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path(), "ws");
    let (gh, log) = fake_gh(dir.path(), &merged_row(8997));
    std::env::set_var(GH_BIN_ENV, &gh);
    let probe = Arc::new(CountingProbe::default());
    let b = cooling(600);
    forge_gate::scoped(gate(&b, &probe), async {
        assert!(fetch_issue_title(&ws, 8997).await.is_none());
        assert!(fetch_merged_pr(&ws, 8997).await.is_none());
        assert!(fetch_repo_identity(&ws).await.is_none());
        assert!(fetch_recent_merged_prs(&ws).await.is_empty());
    })
    .await;
    // Control: the same lookups with no breaker in scope reach `gh`.
    forge_gate::scoped(None, async {
        assert_eq!(fetch_issue_title(&ws, 8997).await.as_deref(), Some("A title"));
    })
    .await;
    std::env::remove_var(GH_BIN_ENV);
    assert_eq!(calls(&log).len(), 1, "only the ungated control call ran: {:?}", calls(&log));
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[serial]
async fn a_trip_in_workspace_a_stops_workspace_b_in_the_same_pass() {
    let dir = tempfile::tempdir().unwrap();
    let ws_a = workspace(dir.path(), "a");
    let ws_b = workspace(dir.path(), "b");
    std::fs::write(ws_a.join(".ratelimited"), "").unwrap();
    let (gh, log) = fake_gh(dir.path(), &merged_row(8997));
    std::env::set_var(GH_BIN_ENV, &gh);
    let probe = Arc::new(CountingProbe::default());
    let b = breaker();
    forge_gate::scoped(gate(&b, &probe), async {
        assert!(fetch_recent_merged_prs(&ws_a).await.is_empty());
        assert!(b.is_suppressed(Utc::now()), "first rate-limited lookup trips");
        assert!(fetch_recent_merged_prs(&ws_b).await.is_empty());
        assert!(fetch_issue_title(&ws_b, 1).await.is_none());
        assert!(fetch_merged_pr(&ws_b, 1).await.is_none());
    })
    .await;
    std::env::remove_var(GH_BIN_ENV);
    let lines = calls(&log);
    assert_eq!(lines.len(), 1, "only workspace A's failing call: {lines:?}");
    // The stub logs bash's `$PWD`, which may be the physical path (macOS
    // `/private/var/…` for a `/var/…` tempdir) depending on how it was spawned.
    let ran_in = Path::new(lines[0].split('|').next().unwrap())
        .canonicalize()
        .unwrap();
    assert_eq!(ran_in, ws_a.canonicalize().unwrap(), "{lines:?}");
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1, "one probe per trip");
    let seen = probe.seen.lock().unwrap();
    assert_eq!(seen[0].root.as_deref(), Some(ws_a.as_path()), "probe keeps A's root");
    assert_eq!(seen[0].program.as_deref(), Some(gh.to_string_lossy().as_ref()));
}

#[tokio::test]
#[serial]
async fn ordinary_failures_and_disabled_breakers_keep_existing_treatment() {
    let dir = tempfile::tempdir().unwrap();
    let ws_a = workspace(dir.path(), "a");
    let ws_b = workspace(dir.path(), "b");
    std::fs::write(ws_a.join(".unauthorized"), "").unwrap();
    let (gh, log) = fake_gh(dir.path(), &merged_row(8997));
    std::env::set_var(GH_BIN_ENV, &gh);
    let probe = Arc::new(CountingProbe::default());
    let b = breaker();
    forge_gate::scoped(gate(&b, &probe), async {
        assert!(fetch_issue_title(&ws_a, 1).await.is_none());
        assert!(!b.is_suppressed(Utc::now()), "a 401 never trips");
        assert_eq!(fetch_issue_title(&ws_b, 1).await.as_deref(), Some("A title"));
    })
    .await;
    // A disabled breaker: rate-limit failures neither trip nor gate.
    std::fs::remove_file(ws_a.join(".unauthorized")).unwrap();
    std::fs::write(ws_a.join(".ratelimited"), "").unwrap();
    let disabled = Arc::new(SharedRateLimitBreaker::new(RateLimitBreakerConfig {
        enabled: false,
        fallback_cooldown_secs: 60,
    }));
    forge_gate::scoped(gate(&disabled, &probe), async {
        assert!(fetch_issue_title(&ws_a, 1).await.is_none());
        assert_eq!(fetch_issue_title(&ws_b, 1).await.as_deref(), Some("A title"));
    })
    .await;
    std::env::remove_var(GH_BIN_ENV);
    assert_eq!(calls(&log).len(), 4);
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[serial]
async fn suppressed_completion_stays_discoverable_and_narrates_once_after_release() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path(), "ws");
    let root = ws.to_string_lossy().into_owned();
    let (gh, log) = fake_gh(dir.path(), &merged_row(8997));
    std::env::set_var(GH_BIN_ENV, &gh);
    let probe = Arc::new(CountingProbe::default());
    let b = cooling(3);
    let mut slug_cache = HashMap::new();
    let mut completed = std::collections::HashSet::new();
    forge_gate::scoped(gate(&b, &probe), async {
        // During cooldown: the SweepExited path and a reconcile pass both
        // skip the forge and record nothing as narrated.
        let exit = completion_for_exit(
            "loom_daemon",
            &root,
            8997,
            60,
            Utc::now(),
            &mut slug_cache,
            &mut completed,
            None,
            None,
        )
        .await;
        assert!(exit.is_none());
        let during = reconcile_recent_merges(
            "loom_daemon",
            &root,
            &mut slug_cache,
            &mut completed,
            None,
            None,
        )
        .await;
        assert!(during.is_empty() && completed.is_empty());
        assert!(calls(&log).is_empty(), "zero forge calls while cooling");

        tokio::time::sleep(Duration::from_millis(3300)).await;
        assert!(!b.is_suppressed(Utc::now()), "window released");

        let after = reconcile_recent_merges(
            "loom_daemon",
            &root,
            &mut slug_cache,
            &mut completed,
            None,
            None,
        )
        .await;
        assert_eq!(after.len(), 1, "the unseen completion is discovered after release");
        assert!(completed.contains(&(root.clone(), 8997, 8997)));
        let again = reconcile_recent_merges(
            "loom_daemon",
            &root,
            &mut slug_cache,
            &mut completed,
            None,
            None,
        )
        .await;
        assert!(again.is_empty(), "release never duplicates a prior narration");
    })
    .await;
    std::env::remove_var(GH_BIN_ENV);
    // After release: pr list + repo view, then the cached repo identity.
    assert_eq!(calls(&log).len(), 3, "{:?}", calls(&log));
}

#[tokio::test]
#[serial]
async fn events_still_flow_untitled_while_cooling() {
    let dir = tempfile::tempdir().unwrap();
    let _paths = SafehouseTestPaths::set(dir.path());
    let ws = workspace(dir.path(), "ws");
    let (gh, log) = fake_gh(dir.path(), "[]");
    std::env::set_var(GH_BIN_ENV, &gh);
    std::env::set_var(DISPATCH_DIGEST_WINDOW_ENV, "10");
    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(stub_server(listener, false, 1));
    let bus = Arc::new(crate::event_bus::EventBus::new());
    let subscription = bus.subscribe(Vec::<String>::new());
    let probe = Arc::new(CountingProbe::default());
    let b = cooling(600);
    let sink = tokio::spawn(forge_gate::scoped(
        gate(&b, &probe),
        run_sink(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket.clone()),
                ..SafehouseConfig::default()
            },
            socket,
            subscription,
            Duration::from_millis(20),
            Duration::from_millis(80),
            new_shared_state(),
            None,
            None,
        ),
    ));
    bus.publish(Event::SweepGlobalDispatch {
        story_points: None,
        sweep_id: "sweep-issue-8997-1".to_owned() as SweepId,
        kind: SweepKind::Issue(8997),
        runtime: None,
        runtime_source: None,
        start: None,
        repo: Some(ws.to_string_lossy().into_owned()),
    })
    .unwrap();
    let received = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("the dispatch narration must still be delivered")
        .unwrap();
    drop(bus);
    let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
    std::env::remove_var(GH_BIN_ENV);
    std::env::remove_var(DISPATCH_DIGEST_WINDOW_ENV);
    assert_eq!(received.len(), 1);
    let body = received[0]["body"].as_str().unwrap();
    assert!(body.contains("#8997 · dispatch") && !body.contains("A title"), "{body:?}");
    assert!(calls(&log).is_empty(), "no title lookup while cooling: {:?}", calls(&log));
}
