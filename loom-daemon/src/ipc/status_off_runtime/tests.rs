//! Tests for the off-runtime `DaemonStatus` build (Issue #10765).

#![allow(clippy::unwrap_used)]

use super::*;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_built_report_becomes_the_status_frame() {
    let response = daemon_status_response(|| DaemonStatusReport {
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
    let response = daemon_status_response(|| panic!("intentional status build panic")).await;
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
/// operator's real daemon files).
fn serve_socket(socket: &Path, root: &Path) {
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
            );
            tokio::spawn(async move {
                let _ = super::super::handle_client(
                    stream, conn.0, conn.1, conn.2, conn.3, conn.4, conn.5, conn.6, conn.7, conn.8,
                )
                .await;
            });
        }
    });
}

/// **Issue #10765 regression.** With the runtime limited to 2 worker threads
/// and 2 slow `DaemonStatus` builds in flight (one per worker — the shape
/// that pinned every worker when the build ran inline), a light request on
/// the same socket must still answer in under 1s, and both status callers
/// must still get their report.
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
    let status_callers: Vec<_> = (0..2)
        .map(|_| {
            let socket = socket.clone();
            std::thread::spawn(move || round_trip(&socket, r#"{"type":"DaemonStatus"}"#))
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
