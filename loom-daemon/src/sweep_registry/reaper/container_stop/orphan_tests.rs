//! A docker probe never outlives its bound or its parent (issue #11449).
//!
//! On 2026-10-11 a host with a wedged Docker backend accumulated 3,641
//! `docker ps --filter label=loom.sweep.issue=<N> …` processes re-parented to
//! pid 1 — most from unit tests that reached the real `docker` through
//! `begin_cancel` and exited before the detached probe thread could kill its
//! child — until `fork` hit the per-user process limit. These tests pin the
//! three layers that close it (see the parent module's docs). Every docker
//! here is a stub script written into the test's own tempdir; nothing touches
//! the host's docker.

use super::{docker_program, list_issue_containers_within, DOCKER_BIN_ENV, TEST_DOCKER_DISABLED};
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a killed process gets to disappear before the test fails.
const GONE_GUARD: Duration = Duration::from_secs(10);

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "loom-docker-orphan-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_stub(dir: &Path, body: &str) -> PathBuf {
    let stub = dir.join("docker");
    std::fs::write(&stub, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    stub
}

/// Read a pid the stub wrote, waiting for the write to land.
fn read_pid(path: &Path) -> i32 {
    let start = Instant::now();
    loop {
        if let Some(pid) = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
        {
            return pid;
        }
        assert!(start.elapsed() < GONE_GUARD, "stub never wrote {}", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// True once `pid` no longer runs: absent, or a zombie awaiting its (new)
/// parent's reap — either way it holds no CPU and executes nothing.
fn gone(pid: i32) -> bool {
    // SAFETY: signal 0 only probes existence.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return true;
    }
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            s.rsplit_once(')')
                .map(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
        .unwrap_or(false)
}

/// Wait for `pid` to be [`gone`]; on failure SIGKILL it so the test itself
/// never leaks what it is asserting about.
fn assert_gone(pid: i32, what: &str) {
    let start = Instant::now();
    while !gone(pid) {
        if start.elapsed() > GONE_GUARD {
            // SAFETY: best-effort cleanup of a pid this test's stub created.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            panic!("{what} (pid {pid}) outlived the probe");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[serial]
fn docker_program_never_resolves_the_real_docker_in_tests() {
    let saved = std::env::var_os(DOCKER_BIN_ENV);
    std::env::remove_var(DOCKER_BIN_ENV);
    let resolved = docker_program();
    if let Some(v) = saved {
        std::env::set_var(DOCKER_BIN_ENV, v);
    }
    assert_eq!(resolved, PathBuf::from(TEST_DOCKER_DISABLED));
    assert_ne!(resolved, PathBuf::from("docker"));
    assert!(!resolved.exists(), "the test fallback must not be a runnable path");
}

#[test]
fn a_hung_docker_ps_is_killed_with_its_descendants_within_the_bound() {
    let dir = tempdir("hung");
    // A wrapper that never answers AND forks a grandchild: `Child::kill`
    // alone would leave the background `sleep` running.
    let stub = write_stub(
        &dir,
        &format!(
            "echo $$ > {d}/pid\nsleep 1000 &\necho $! > {d}/child\nwait\n",
            d = dir.display()
        ),
    );
    let timeout = Duration::from_secs(1);
    let start = Instant::now();
    let listed = list_issue_containers_within(&stub, 4242, timeout);
    let elapsed = start.elapsed();
    assert!(listed.is_empty(), "a timed-out probe must fail safe to no containers");
    assert!(
        elapsed < timeout + Duration::from_secs(1),
        "the hung probe must be abandoned within timeout + 1s, took {elapsed:?}"
    );
    assert_gone(read_pid(&dir.join("pid")), "the hung docker stub");
    assert_gone(read_pid(&dir.join("child")), "the stub's background child");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Env var that turns [`a_probe_dies_with_its_parent_process`] into its own
/// helper when the test binary re-executes itself.
#[cfg(target_os = "linux")]
const HELPER_ENV: &str = "LOOM_TEST_11449_PDEATHSIG_HELPER";

#[cfg(target_os = "linux")]
#[test]
fn a_probe_dies_with_its_parent_process() {
    if let Some(dir) = std::env::var_os(HELPER_ENV) {
        // Helper mode: start a probe that will never answer, on a detached
        // thread exactly as the teardown does, then exit the whole process
        // while it is still blocked — the unit-test-binary orphaning case.
        let dir = PathBuf::from(dir);
        let stub = dir.join("docker");
        std::thread::spawn(move || {
            list_issue_containers_within(&stub, 4242, Duration::from_secs(600));
        });
        read_pid(&dir.join("pid"));
        std::process::exit(0);
    }

    let dir = tempdir("pdeathsig");
    // `exec` keeps the probe's pid, so the recorded pid IS the probe child.
    write_stub(&dir, &format!("echo $$ > {}/pid\nexec sleep 1000\n", dir.display()));
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "sweep_registry::reaper::container_stop::orphan_tests::a_probe_dies_with_its_parent_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER_ENV, &dir)
        .env_remove(DOCKER_BIN_ENV)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "helper process failed: {status:?}");
    assert_gone(read_pid(&dir.join("pid")), "the probe whose parent exited");
    let _ = std::fs::remove_dir_all(&dir);
}
