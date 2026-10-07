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
//! records its argv and then blocks on a *gate file* the test releases only
//! after every assertion about the stalled window has been made. No pass
//! condition here is a wall-clock latency bound (those flake under host load):
//! each is "this completed while docker was provably still blocked", with only
//! a large hang guard as the failure path. **Nothing here touches the host's docker service, a real
//! container, a credential or a model** — the stall is a poll loop in a script this
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
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

/// Hang guard (milliseconds) for every wait on an event the test expects to
/// happen. Never a pass condition: it only bounds how long a regression (or a
/// wedged host) can keep the test alive before it fails.
const HANG_GUARD_MS: u64 = 60_000;

/// Hang guard for channel receives.
const HANG_GUARD: Duration = Duration::from_millis(HANG_GUARD_MS);

/// Cancellation grace the fixture drives the split cancel with.
const GRACE: Duration = Duration::from_millis(1_000);

/// Container the fake `docker ps` reports once its stall elapses, so the
/// begin/finish halves still exercise the real `stop`/`kill` argv after the
/// move off the caller's thread.
const FAKE_CONTAINER: &str = "deadbeef9c01";

/// Write a fake docker CLI that records every invocation's argv and, for `ps`,
/// blocks until `<dir>/release` exists before reporting [`FAKE_CONTAINER`]
/// (bounded at ~120s so a leaked process cannot live forever). `stop`/`kill`
/// return immediately — the point of the fixture is a wedged *discovery*.
///
/// The argv record is written BEFORE the gate so a test can observe that the
/// probe started without waiting for it to finish.
fn stalling_docker(dir: &Path) -> PathBuf {
    let script = dir.join("fake-docker");
    std::fs::write(
        &script,
        format!(
            "#!/bin/bash\n\
             printf '%s\\n' \"$*\" >> {dir}/invocations\n\
             if [[ \"$1\" == ps ]]; then\n\
             \x20 for ((i = 0; i < 6000; i++)); do [[ -e {dir}/release ]] && break; sleep 0.02; done\n\
             \x20 printf '%s\\t%s\\n' '{id}' 'claude-ephemeral'\n\
             fi\n\
             exit 0\n",
            dir = dir.display(),
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
        overflow: false,
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

    // Opens the gate (and thereby unblocks any leaked fake `ps`) on every exit
    // path, including a failed assertion.
    struct OpenGate(PathBuf);
    impl Drop for OpenGate {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, b"");
        }
    }
    let release = fake_dir.join("release");
    let _gate = OpenGate(release.clone());

    // A real child that survives SIGTERM (so the cancel is forced to poll the
    // full grace and escalate) but records the moment it received one. It
    // touches `ready` only after its TERM trap is installed.
    let termed = dir.path().join("termed");
    let ready = dir.path().join("ready");
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "trap 'touch {}' TERM; touch {}; while true; do sleep 0.05; done",
            termed.display(),
            ready.display()
        ))
        .spawn()
        .expect("spawn fixture child");
    let target_pid = child.id();
    let ready_seen = wait_for_condition(HANG_GUARD_MS, || ready.exists());
    if !ready_seen {
        let _ = child.kill();
        let _ = child.wait();
    }
    assert!(ready_seen, "fixture child never installed its TERM trap");

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
    // drives it (#3807) — lock, begin, unlock, poll, lock, finish. It reports
    // each half's completion over channels so the main thread can assert
    // "completed while docker was still blocked" without a stopwatch.
    let reg_a = Arc::clone(&registry);
    let target_a = target.clone();
    let (begin_tx, begin_rx) = mpsc::channel::<()>();
    let (finish_tx, finish_rx) = mpsc::channel();
    let canceller = thread::spawn(move || {
        let (pid, kind, started_at) = match reg_a.lock().unwrap().begin_cancel(&target_a, GRACE) {
            Ok(BeginCancel::Signalled {
                pid,
                kind,
                started_at,
            }) => (pid, kind, started_at),
            unexpected => panic!("target should have been running: {unexpected:?}"),
        };
        let _ = begin_tx.send(());

        let deadline = Instant::now() + GRACE;
        let mut exited = reg_a.lock().unwrap().poll_cancel(&target_a, pid);
        while !exited && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
            exited = reg_a.lock().unwrap().poll_cancel(&target_a, pid);
        }

        let outcome = reg_a
            .lock()
            .unwrap()
            .finish_cancel(&target_a, pid, &kind, started_at, exited);
        let _ = finish_tx.send(());
        outcome
    });

    // Everything below runs with the docker gate CLOSED: if discovery were
    // inline under the registry mutex, the step in question would block until
    // the hang guard and the test would fail.
    let mut fail = |what: &str| -> ! {
        let log = invocations(&fake_dir);
        let _ = std::fs::write(&release, b"");
        let _ = child.kill();
        panic!("{what} while `docker ps` was blocked (#8776); docker invocations: {log}");
    };

    // AC2: the host SIGTERM is delivered even though `docker ps` is wedged.
    // Observed on the CHILD — this is delivery, not intent.
    if !wait_for_condition(HANG_GUARD_MS, || termed.exists()) {
        fail("fixture child never saw SIGTERM: docker discovery delayed cancellation signalling");
    }

    // begin_cancel returned (did not hold the caller across discovery).
    if begin_rx.recv_timeout(HANG_GUARD).is_err() {
        fail("begin_cancel never returned: discovery is back on the caller's thread");
    }

    // AC1: an unrelated sweep's status read completes while the cancel is in
    // flight and the gate is closed. Run on a helper thread so a mutex held
    // across discovery shows up as a hang-guard timeout, not a deadlock.
    let (read_tx, read_rx) = mpsc::channel();
    let reg_r = Arc::clone(&registry);
    let other_r = other.clone();
    thread::spawn(move || {
        let _ = read_tx.send(reg_r.lock().unwrap().get_status(&other_r));
    });
    match read_rx.recv_timeout(HANG_GUARD) {
        Ok(info) => assert!(info.is_some(), "the unrelated sweep should still be queryable"),
        Err(_) => fail("an unrelated get_status never completed: the registry mutex is held across docker discovery"),
    }

    // finish_cancel returned too (the escalation half does not re-list inline).
    if finish_rx.recv_timeout(HANG_GUARD).is_err() {
        fail("finish_cancel never returned: the escalation half re-lists inline");
    }
    let outcome = canceller.join().expect("cancel thread panicked");
    assert!(outcome.was_running);
    assert!(
        outcome.sigkill_sent,
        "a TERM-trapping child should have survived the grace and escalated to SIGKILL"
    );

    // Discovery really was in flight, and still blocked, during all of the above.
    if !wait_for_condition(HANG_GUARD_MS, || invocations(&fake_dir).contains("ps ")) {
        fail("no `docker ps` was ever started");
    }
    assert!(
        !release.exists(),
        "the gate must still be closed when the stalled-window assertions finish"
    );
    assert!(
        !invocations(&fake_dir).contains("stop --time 1"),
        "docker stop ran before discovery was released: {}",
        invocations(&fake_dir)
    );

    // Release the gate: discovery may now finish and the teardown proceed.
    std::fs::write(&release, b"").unwrap();

    // Reap the killed child so its pid leaves the process table.
    let _ = child.wait();

    // AC3/AC4: moving discovery off the caller's thread must not DROP the
    // teardown. Both halves still discover by label and still issue their
    // bounded `stop` / `kill` — just later, and off the lock. The budget is a
    // hang guard only.
    let budget = HANG_GUARD_MS;
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
