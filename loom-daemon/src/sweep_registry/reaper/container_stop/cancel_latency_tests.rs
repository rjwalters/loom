//! Cancellation latency when the docker CLI is unresponsive (issue #8776).
//!
//! The regression these pin is not "the container gets stopped" (that is
//! `container_stop::tests`' job) but **who waits for docker**. Both teardown
//! halves are invoked from inside the registry's critical section, and
//! [`begin_cancel`](crate::sweep_registry::SweepRegistry::begin_cancel) invokes
//! the begin half *before* it delivers the host process-group SIGTERM. While
//! discovery (`docker ps`) ran on the caller's thread, a wedged dockerd held
//! the registry mutex for the whole `reap_gh_timeout()` budget — an unrelated
//! `get_status` measured ~4.86s behind a 1s cancellation grace on 2026-09-23,
//! and the SIGTERM went out behind it too.
//!
//! The fake docker CLI below makes that deterministic and hermetic: its `ps`
//! records its argv and then stalls for [`PS_STALL`], far past the fixture's
//! cancellation grace. **Nothing here touches the host's docker service, a real
//! container, a credential or a model** — the stall is `sleep` in a script this
//! test writes into its own tempdir, so the assertions hold identically on a
//! host with no docker installed, a healthy one, and a wedged one.

use super::DOCKER_BIN_ENV;
use crate::sweep_registry::test_support::{fixture_registry, wait_for_condition};
use crate::sweep_registry::BeginCancel;
use crate::types::{SweepInfo, SweepKind, SweepState};
use chrono::Utc;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

/// How long the fake `docker ps` stalls. Must be comfortably longer than
/// [`GRACE`] *and* than every latency bound asserted below, so a regression
/// that puts discovery back on the caller's thread cannot pass by luck.
const PS_STALL: Duration = Duration::from_millis(2_500);

/// Cancellation grace the fixture drives the split cancel with.
const GRACE: Duration = Duration::from_millis(1_000);

/// Latency ceiling for the two lock-scoped cancel steps and for host SIGTERM
/// delivery. Generous next to their real cost (a thread spawn plus a `kill(2)`,
/// i.e. sub-millisecond) but a third of [`PS_STALL`], so "discovery ran inline"
/// is unambiguously distinguishable from "the host was briefly busy".
const PROMPT: Duration = Duration::from_millis(800);

/// Container the fake `docker ps` reports once its stall elapses, so the
/// begin/finish halves still exercise the real `stop`/`kill` argv after the
/// move off the caller's thread.
const FAKE_CONTAINER: &str = "deadbeef9c01";

/// Write a fake docker CLI that records every invocation's argv and, for `ps`,
/// stalls [`PS_STALL`] before reporting [`FAKE_CONTAINER`]. `stop`/`kill`
/// return immediately — the point of the fixture is a wedged *discovery*.
///
/// The argv record is written BEFORE the stall so a test can observe that the
/// probe started without waiting for it to finish.
fn stalling_docker(dir: &Path) -> PathBuf {
    let script = dir.join("fake-docker");
    std::fs::write(
        &script,
        format!(
            "#!/bin/bash\n\
             printf '%s\\n' \"$*\" >> {dir}/invocations\n\
             if [[ \"$1\" == ps ]]; then\n\
             \x20 sleep {stall}\n\
             \x20 printf '%s\\t%s\\n' '{id}' 'claude-ephemeral'\n\
             fi\n\
             exit 0\n",
            dir = dir.display(),
            stall = PS_STALL.as_secs_f32(),
            id = FAKE_CONTAINER,
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn invocations(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("invocations")).unwrap_or_default()
}

/// A registry entry for a sweep the fixture injects directly (no dispatch).
/// `pgid: None` plus no retained `Child` handle means `signal_sweep` degrades
/// to single-PID delivery — it can never reach the test process's own group.
fn entry(sweep_id: &str, issue: u32, pid: u32, log_path: PathBuf) -> SweepInfo {
    SweepInfo {
        pgid: None,
        sweep_id: sweep_id.to_string(),
        kind: SweepKind::Issue(issue),
        pid,
        token_name: "unknown".into(),
        runtime: "unknown".into(),
        runtime_source: None,
        log_path,
        idempotency_key: None,
        started_at: Utc::now(),
        state: SweepState::Running,
        latest_phase: None,
        pr_number: None,
        model: None,
        effort: None,
        depends_on: None,
        repo: None,
    }
}

/// A stalled `docker ps` must not delay the host SIGTERM, must not hold the
/// registry mutex, and must not stop the teardown from eventually running.
///
/// `#[serial]`: the docker binary override is process-global. Under nextest
/// (how CI runs this suite) each test is its own process and the marker is a
/// no-op; it is load-bearing only for a developer running plain `cargo test`.
#[test]
#[serial]
fn stalled_docker_discovery_delays_neither_sigterm_nor_unrelated_reads() {
    let dir = tempdir().unwrap();
    let fake_dir = dir.path().join("docker");
    std::fs::create_dir_all(&fake_dir).unwrap();
    let fake = stalling_docker(&fake_dir);
    // SAFETY-of-scope: restored at the end of the test; see `#[serial]` above.
    std::env::set_var(DOCKER_BIN_ENV, &fake);

    let (registry, _record_log) = fixture_registry(dir.path());
    let registry = Arc::new(Mutex::new(registry));

    // A real child that survives SIGTERM (so the cancel is forced to poll the
    // full grace and escalate) but records the moment it received one.
    let termed = dir.path().join("termed");
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "trap 'touch {}' TERM; while true; do sleep 0.05; done",
            termed.display()
        ))
        .spawn()
        .expect("spawn fixture child");
    let target_pid = child.id();
    // Let bash install the trap before anything TERMs it.
    thread::sleep(Duration::from_millis(150));

    let target = "sweep-cancel-stalled-docker".to_string();
    let other = "sweep-unrelated-reader".to_string();
    {
        let mut reg = registry.lock().unwrap();
        let target_log = reg.compute_log_path(8776);
        let other_log = reg.compute_log_path(8777);
        reg.entries
            .insert(target.clone(), entry(&target, 8776, target_pid, target_log));
        reg.entries.insert(
            other.clone(),
            // ~i32::MAX: a harmless dead pid, so the unrelated read never
            // depends on a live process.
            entry(&other, 8777, 2_147_483_640, other_log),
        );
    }

    // Thread A: the split cancel exactly as the non-blocking IPC handler
    // drives it (#3807) — lock, begin, unlock, poll, lock, finish.
    let reg_a = Arc::clone(&registry);
    let target_a = target.clone();
    let started = Instant::now();
    let canceller = thread::spawn(move || {
        let begin_started = Instant::now();
        let (pid, kind, started_at) = match reg_a.lock().unwrap().begin_cancel(&target_a, GRACE) {
            Ok(BeginCancel::Signalled {
                pid,
                kind,
                started_at,
            }) => (pid, kind, started_at),
            unexpected => panic!("target should have been running: {unexpected:?}"),
        };
        let begin_elapsed = begin_started.elapsed();

        let deadline = Instant::now() + GRACE;
        let mut exited = reg_a.lock().unwrap().poll_cancel(&target_a, pid);
        while !exited && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
            exited = reg_a.lock().unwrap().poll_cancel(&target_a, pid);
        }

        let finish_started = Instant::now();
        let outcome = reg_a
            .lock()
            .unwrap()
            .finish_cancel(&target_a, pid, &kind, started_at, exited);
        (begin_elapsed, finish_started.elapsed(), outcome)
    });

    // AC2: the host SIGTERM goes out promptly even though `docker ps` is
    // wedged for PS_STALL. Measured on the CHILD, not on our own call — this
    // is delivery, not intent.
    let term_seen =
        wait_for_condition(u64::try_from(PROMPT.as_millis()).unwrap(), || termed.exists());
    let term_latency = started.elapsed();
    assert!(
        term_seen,
        "fixture child never saw SIGTERM within {PROMPT:?} — docker discovery \
         delayed cancellation signalling (#8776); docker invocations so far: {}",
        invocations(&fake_dir)
    );

    // AC1: an unrelated sweep's status read stays prompt while the cancel is
    // in flight and the fake `docker ps` is still stalled.
    let read_started = Instant::now();
    let info = registry.lock().unwrap().get_status(&other);
    let read_elapsed = read_started.elapsed();
    assert!(info.is_some(), "the unrelated sweep should still be queryable");
    assert!(
        read_elapsed < Duration::from_millis(400),
        "an unrelated get_status blocked for {read_elapsed:?} while cancellation was in \
         progress — the registry mutex was held across docker discovery (stall is \
         {PS_STALL:?}, grace {GRACE:?}) (#8776)"
    );

    let (begin_elapsed, finish_elapsed, outcome) =
        canceller.join().expect("cancel thread panicked");
    assert!(
        begin_elapsed < PROMPT,
        "begin_cancel held the registry mutex for {begin_elapsed:?} against a {PS_STALL:?} \
         `docker ps` stall — discovery is back on the caller's thread (#8776)"
    );
    assert!(
        finish_elapsed < PROMPT,
        "finish_cancel held the registry mutex for {finish_elapsed:?} against a {PS_STALL:?} \
         `docker ps` stall — the escalation half re-lists inline (#8776)"
    );
    assert!(outcome.was_running);
    assert!(
        outcome.sigkill_sent,
        "a TERM-trapping child should have survived the grace and escalated to SIGKILL"
    );
    assert!(
        term_latency < PROMPT,
        "SIGTERM reached the child only after {term_latency:?} (#8776)"
    );

    // Reap the killed child so its pid leaves the process table.
    let _ = child.wait();

    // AC3/AC4: moving discovery off the caller's thread must not DROP the
    // teardown. Both halves still discover by label and still issue their
    // bounded `stop` / `kill` — just later, and off the lock. Budget covers
    // both stalls back to back plus scheduling slop.
    let budget = u64::try_from(PS_STALL.as_millis()).unwrap() * 3 + 5_000;
    let stop_seen = wait_for_condition(budget, || {
        invocations(&fake_dir).contains(&format!("stop --time 1 {FAKE_CONTAINER}"))
    });
    assert!(
        stop_seen,
        "the begin half never issued its docker stop: {}",
        invocations(&fake_dir)
    );
    let kill_seen = wait_for_condition(budget, || {
        invocations(&fake_dir).contains(&format!("kill {FAKE_CONTAINER}"))
    });
    assert!(
        kill_seen,
        "the finish half never escalated to docker kill: {}",
        invocations(&fake_dir)
    );
    let log = invocations(&fake_dir);
    assert_eq!(
        log.matches("ps --filter label=loom.sweep.issue=8776 --filter")
            .count(),
        2,
        "each half must discover by the issue+dispatch label filter, once: {log}"
    );

    std::env::remove_var(DOCKER_BIN_ENV);
}
