//! Tests for the off-runtime `DaemonStatus` build (Issue #10765) through the
//! real connection handler. The single-flight logic itself (#10861) is
//! covered with injected builds in `flight_tests.rs`.

#![allow(clippy::unwrap_used)]

use super::*;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One unshared build through the single-flight path, as its reply frame.
async fn response_for<F>(build: F) -> Response
where
    F: FnOnce() -> DaemonStatusReport + Send + 'static,
{
    let flights = Arc::new(StatusFlights::default());
    let outcome = single_flight(&flights, SectionSet::all(), build).await;
    reply(outcome, &DrainState::new())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_built_report_becomes_the_status_frame() {
    let response = response_for(|| DaemonStatusReport {
        configured_max: 7,
        ..DaemonStatusReport::default()
    })
    .await;
    match response {
        Response::DaemonStatus(report) => assert_eq!(report.configured_max, 7),
        other => panic!("expected DaemonStatus, got {other:?}"),
    }
}

/// #4279 kept: a panic inside the build (now on the blocking pool) still
/// produces an explicit error frame naming the cause, never a dropped socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_build_still_yields_an_error_frame() {
    let response = response_for(|| panic!("intentional status build panic")).await;
    match response {
        Response::Error { message } => {
            assert!(message.contains("daemon failed to build status report"), "{message}");
            assert!(message.contains("intentional status build panic"), "{message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

/// One blocking request/response round-trip over the daemon socket, on the
/// calling OS thread (never a tokio worker), with a generous read timeout so
/// a starved daemon shows as a slow answer rather than a hang.
fn round_trip(socket: &Path, line: &str) -> String {
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream.write_all(line.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).unwrap();
    reply
}

/// Serve `socket` with the real per-connection handler, as `IpcServer::run`
/// does, minus its singleton guard and pid-file claim (which would touch the
/// operator's real daemon files). Returns the server's flight registry.
fn serve_socket(socket: &Path, root: &Path) -> Arc<StatusFlights> {
    use crate::activity::ActivityDb;
    use crate::event_bus::EventBus;
    use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
    use crate::terminal::TerminalManager;

    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let bus = Arc::new(EventBus::new());
    let mut config = SweepRegistryConfig::new(root.to_path_buf());
    config.skip_label_flip = true;
    let mut registry = SweepRegistry::new(config);
    registry.set_event_bus(bus.clone());
    let registry = Arc::new(Mutex::new(registry));
    let pool = Arc::new(WorkspacePool::new(bus.clone(), tokio::runtime::Handle::current()));
    pool.seed(root.to_path_buf(), registry.clone());
    let terminals = Arc::new(Mutex::new(TerminalManager::new()));
    let activity = Arc::new(Mutex::new(ActivityDb::new(root.join("activity.db")).unwrap()));
    let health = Arc::new(WorkspaceHealthStates::new());
    let credentials = Arc::new(CredentialPreflightReport {
        ok: true,
        mechanism: "test-fixture".to_string(),
        fingerprint: None,
        message: "test fixture".to_string(),
        checked_at: chrono::Utc::now(),
        pool: None,
    });
    let drain = Arc::new(DrainState::new());
    let flights = Arc::new(StatusFlights::default());
    let served = flights.clone();
    let root: PathBuf = root.to_path_buf();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let conn = (
                terminals.clone(),
                activity.clone(),
                registry.clone(),
                bus.clone(),
                health.clone(),
                pool.clone(),
                root.clone(),
                credentials.clone(),
                drain.clone(),
                flights.clone(),
            );
            tokio::spawn(async move {
                let _ = super::super::handle_client(
                    stream, conn.0, conn.1, conn.2, conn.3, conn.4, conn.5, conn.6, conn.7, conn.8,
                    conn.9,
                )
                .await;
            });
        }
    });
    served
}

/// A one-repo workspace served on a socket, with the registry and shared
/// token-dir env pointed away from the operator's real files and every status
/// build held for `build_delay`. Restores all of it on drop, panics included.
struct ServedRepo {
    dir: tempfile::TempDir,
    flights: Arc<StatusFlights>,
    prev_shared: Option<String>,
}

impl ServedRepo {
    fn start(build_delay: Duration) -> Self {
        use crate::workspace_registry::REGISTRY_PATH_ENV;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join(".loom")).unwrap();
        std::env::set_var(REGISTRY_PATH_ENV, dir.path().join("no-such-workspaces.json"));
        let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");
        TEST_BUILD_DELAY_MS
            .store(u64::try_from(build_delay.as_millis()).unwrap(), Ordering::SeqCst);
        let flights = serve_socket(&dir.path().join("daemon.sock"), &root);
        Self {
            dir,
            flights,
            prev_shared,
        }
    }

    /// `n` concurrent requests, each on its own connection and OS thread.
    fn ask_concurrently(
        &self,
        n: usize,
        line: &'static str,
    ) -> Vec<std::thread::JoinHandle<String>> {
        (0..n)
            .map(|_| {
                let socket = self.dir.path().join("daemon.sock");
                std::thread::spawn(move || round_trip(&socket, line))
            })
            .collect()
    }

    fn builds_started(&self) -> usize {
        self.flights.builds_started.load(Ordering::SeqCst)
    }
}

impl Drop for ServedRepo {
    fn drop(&mut self) {
        TEST_BUILD_DELAY_MS.store(0, Ordering::SeqCst);
        std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
        match self.prev_shared.take() {
            Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
            None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
        }
    }
}

/// The replies of `callers`, joined off the runtime's workers.
async fn replies(callers: Vec<std::thread::JoinHandle<String>>) -> Vec<Response> {
    let mut replies = Vec::new();
    for caller in callers {
        let line = tokio::task::spawn_blocking(move || caller.join().unwrap())
            .await
            .unwrap();
        replies.push(serde_json::from_str(&line).unwrap());
    }
    replies
}

/// **Issue #10861, end to end.** Four `DaemonStatus` requests on four
/// connections while a build is held run one build and all get its report; a
/// request after it has landed starts a second (no reuse window).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn concurrent_status_requests_on_the_socket_share_one_build() {
    let served = ServedRepo::start(Duration::from_secs(3));

    let callers = served.ask_concurrently(4, r#"{"type":"DaemonStatus"}"#);
    for reply in replies(callers).await {
        assert!(matches!(reply, Response::DaemonStatus(_)), "status reply: {reply:?}");
    }
    assert_eq!(served.builds_started(), 1, "four concurrent requests started several builds");

    TEST_BUILD_DELAY_MS.store(0, Ordering::SeqCst);
    let later = replies(served.ask_concurrently(1, r#"{"type":"DaemonStatus"}"#)).await;
    assert!(matches!(later[0], Response::DaemonStatus(_)), "{later:?}");
    assert_eq!(served.builds_started(), 2, "a request after a finished build must rebuild");

    // A wire request naming no section is refused, not built (#10879 nit).
    let empty = replies(
        served.ask_concurrently(1, r#"{"type":"DaemonStatusSections","payload":{"sections":[]}}"#),
    )
    .await;
    match &empty[0] {
        Response::Error { message } => {
            assert!(message.contains("must name at least one section"), "{message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
    assert_eq!(served.builds_started(), 2, "an empty section list started a build");
}

/// **Issue #10861 metrics.** With 8 requests sharing one build,
/// `loom.daemon.ipc.requests{kind=DaemonStatus}` still counts every request
/// and `loom.daemon.ipc.status_builds{outcome=ok}` counts the one build.
///
/// A current-thread runtime, so the connection handlers and the detached
/// build task all record on this thread, where the capture is listening.
#[test]
#[serial_test::serial]
fn eight_shared_requests_count_eight_requests_and_one_build() {
    use crate::observability::ops::{capture::capture, ipc_latency};
    use crate::telemetry::ops::{MetricName, MetricValue};

    let (points, _) = capture(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let served = ServedRepo::start(Duration::from_secs(3));
            let callers = served.ask_concurrently(8, r#"{"type":"DaemonStatus"}"#);
            for reply in replies(callers).await {
                assert!(matches!(reply, Response::DaemonStatus(_)), "status reply: {reply:?}");
            }
            assert_eq!(served.builds_started(), 1);
        });
        ipc_latency::drain_points()
    });
    let value = |name: MetricName, key: &str, label: &str| {
        points
            .iter()
            .find(|p| p.name == name && p.labels.get(key).map(String::as_str) == Some(label))
            .map(|p| p.value)
    };
    assert_eq!(
        value(MetricName::DaemonIpcRequests, "kind", "DaemonStatus"),
        Some(MetricValue::Int(8)),
        "latency stays per request: {points:?}"
    );
    assert!(value(MetricName::DaemonIpcLatency, "kind", "DaemonStatus").is_some());
    assert_eq!(
        value(MetricName::DaemonIpcStatusBuilds, "outcome", "ok"),
        Some(MetricValue::Int(1)),
        "{points:?}"
    );
}

/// **Issue #10765 regression.** With the runtime limited to 2 worker threads
/// and 2 slow `DaemonStatus` builds in flight (one per worker — the shape
/// that pinned every worker when the build ran inline), a light request on
/// the same socket must still answer in under 1s, and both status callers
/// must still get their report. The two callers ask for different section
/// sets, so they stay two builds now that same-set requests share one
/// (#10861).
///
/// Before #10765 this fails: both workers sit inside the synchronous build,
/// so the accept loop and the light connection's handler cannot run until a
/// build returns (the light call took about as long as the remaining build).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_light_request_answers_while_slow_status_builds_occupy_every_worker() {
    use crate::workspace_registry::REGISTRY_PATH_ENV;

    const BUILD_DELAY: Duration = Duration::from_secs(4);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::env::set_var(REGISTRY_PATH_ENV, dir.path().join("no-such-workspaces.json"));
    let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");
    let socket = dir.path().join("daemon.sock");
    serve_socket(&socket, &root);

    /// Resets the process-global build delay even if this test panics.
    struct ResetBuildDelay;
    impl Drop for ResetBuildDelay {
        fn drop(&mut self) {
            TEST_BUILD_DELAY_MS.store(0, Ordering::SeqCst);
        }
    }
    TEST_BUILD_DELAY_MS.store(u64::try_from(BUILD_DELAY.as_millis()).unwrap(), Ordering::SeqCst);
    let reset_delay = ResetBuildDelay;
    let status_callers: Vec<_> = [
        r#"{"type":"DaemonStatus"}"#,
        r#"{"type":"DaemonStatusSections","payload":{"sections":["per_repo"]}}"#,
    ]
    .into_iter()
    .map(|line| {
        let socket = socket.clone();
        std::thread::spawn(move || round_trip(&socket, line))
    })
    .collect();
    // Let both builds get past the CPU-sample pre-warm and into the delay.
    std::thread::sleep(BUILD_DELAY / 2);

    let started = Instant::now();
    let light = round_trip(&socket, r#"{"type":"ListWorkspaces"}"#);
    let light_elapsed = started.elapsed();

    let status_replies: Vec<String> = status_callers
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect();
    drop(reset_delay);
    std::env::remove_var(REGISTRY_PATH_ENV);
    match prev_shared {
        Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
        None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
    }

    let light: Response = serde_json::from_str(&light).unwrap();
    assert!(!matches!(light, Response::Error { .. }), "light request failed: {light:?}");
    assert!(
        light_elapsed < Duration::from_secs(1),
        "a light request took {light_elapsed:?} while status builds ran — the builds are \
         starving the tokio workers again (#10765)"
    );
    for reply in status_replies {
        let reply: Response = serde_json::from_str(&reply).unwrap();
        assert!(matches!(reply, Response::DaemonStatus(_)), "status reply: {reply:?}");
    }
}

/// **Issue #10787.** `DaemonStatusSections` through the real connection
/// handler: the reply is a `DaemonStatus` frame whose requested sections are
/// built while the per-repo rows (which nothing requested) are not, and the
/// plain `DaemonStatus` frame on the same socket still builds them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_sectioned_request_builds_only_what_it_names() {
    use crate::workspace_registry::REGISTRY_PATH_ENV;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::env::set_var(REGISTRY_PATH_ENV, dir.path().join("no-such-workspaces.json"));
    let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");
    let socket = dir.path().join("daemon.sock");
    serve_socket(&socket, &root);

    let ask = |line: &'static str| {
        let socket = socket.clone();
        tokio::task::spawn_blocking(move || round_trip(&socket, line))
    };
    let sectioned = ask(
        r#"{"type":"DaemonStatusSections","payload":{"sections":["daemon_build","auto_update"]}}"#,
    )
    .await
    .unwrap();
    let full = ask(r#"{"type":"DaemonStatus"}"#).await.unwrap();
    let unknown =
        ask(r#"{"type":"DaemonStatusSections","payload":{"sections":["no_such_section"]}}"#)
            .await
            .unwrap();
    std::env::remove_var(REGISTRY_PATH_ENV);
    match prev_shared {
        Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
        None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
    }

    match serde_json::from_str::<Response>(&sectioned).unwrap() {
        Response::DaemonStatus(report) => {
            assert_eq!(
                report.daemon_build_commit.as_deref(),
                Some(crate::self_update::BUILT_COMMIT)
            );
            assert!(report.per_repo.is_empty(), "per-repo rows built for {sectioned}");
        }
        other => panic!("expected DaemonStatus, got {other:?}"),
    }
    match serde_json::from_str::<Response>(&full).unwrap() {
        Response::DaemonStatus(report) => assert_eq!(report.per_repo.len(), 1),
        other => panic!("expected DaemonStatus, got {other:?}"),
    }
    // A section this daemon does not know is a parse error frame — what a
    // newer CLI maps to "daemon too old for --section" — not a full build.
    assert!(
        matches!(
            serde_json::from_str::<Response>(&unknown).unwrap(),
            Response::StructuredError(_)
        ),
        "unknown section reply: {unknown}"
    );
}
