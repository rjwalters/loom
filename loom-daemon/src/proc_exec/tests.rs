//! Tests for the bounded execution boundary (epic #7810, PR 1).
//!
//! These test the boundary *directly* rather than through a consumer. Every
//! case below is a distinction the pre-extraction executor could not make, or
//! made wrongly — so each one is a regression test with a known failing
//! predecessor, not a restatement of the implementation.

use super::*;
use std::process::Command;
use std::time::{Duration, Instant};

const GENEROUS: Duration = Duration::from_secs(10);

#[test]
fn a_successful_command_reports_its_bytes_and_zero_status() {
    let mut cmd = Command::new("/bin/echo");
    cmd.arg("hi");
    let c = run_bounded(cmd, GENEROUS).expect("echo should spawn");
    match c {
        Completion::Exited(o) => {
            assert!(o.status.success());
            assert_eq!(o.stdout, b"hi\n");
        }
        Completion::TimedOut { .. } => panic!("a fast command must not time out"),
    }
}

#[test]
fn a_nonzero_exit_is_a_completed_execution_not_a_failure_to_execute() {
    // The distinction the whole module exists for: `false` ran perfectly well.
    // Collapsing this into an error (or into GhResult's `success: bool`) is how
    // "the command failed" and "we could not ask" become the same thing.
    let cmd = Command::new("/bin/sh");
    let mut cmd = cmd;
    cmd.args(["-c", "exit 3"]);
    let c = run_bounded(cmd, GENEROUS).expect("sh should spawn");
    match c {
        Completion::Exited(o) => {
            assert!(!o.status.success());
            assert_eq!(o.status.code(), Some(3), "the exact exit code must survive");
        }
        Completion::TimedOut { .. } => panic!("exit 3 is immediate"),
    }
}

#[test]
fn stderr_is_captured_separately_from_stdout() {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "echo out; echo err >&2"]);
    let c = run_bounded(cmd, GENEROUS).expect("sh should spawn");
    let Completion::Exited(o) = c else {
        panic!("must complete")
    };
    assert_eq!(o.stdout, b"out\n");
    assert_eq!(o.stderr, b"err\n");
}

#[test]
fn a_missing_executable_is_a_spawn_error_not_a_timeout_and_not_an_exit() {
    let cmd = Command::new("/nonexistent/loom/definitely-not-here");
    match run_bounded(cmd, GENEROUS) {
        Err(ExecError::Spawn(e)) => {
            assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
        }
        Err(ExecError::Collect(e)) => panic!("a missing binary never started: {e}"),
        Ok(c) => panic!("a missing binary cannot produce a completion: {c:?}"),
    }
}

#[test]
#[cfg(unix)]
fn signal_termination_is_preserved_in_the_exit_status() {
    use std::os::unix::process::ExitStatusExt;
    // The child kills itself, so this is the CHILD's fate, not ours — it must
    // arrive as a completed execution carrying the signal, distinct from the
    // TimedOut case where the signal was our doing.
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "kill -TERM $$; sleep 5"]);
    let c = run_bounded(cmd, GENEROUS).expect("sh should spawn");
    match c {
        Completion::Exited(o) => {
            assert_eq!(o.status.code(), None, "a signalled process has no exit code");
            assert_eq!(o.status.signal(), Some(libc::SIGTERM), "the signal must survive");
        }
        Completion::TimedOut { .. } => panic!("the child died well inside the budget"),
    }
}

#[test]
fn non_utf8_output_survives_as_raw_bytes() {
    // Anything that decodes on the way out (String::from_utf8_lossy at the
    // boundary) silently corrupts binary payloads. Output must stay Vec<u8>.
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", r"printf 'a\377b'"]);
    let c = run_bounded(cmd, GENEROUS).expect("sh should spawn");
    let Completion::Exited(o) = c else {
        panic!("must complete")
    };
    assert_eq!(
        o.stdout,
        vec![b'a', 0xFF, b'b'],
        "0xFF is not valid UTF-8 and must not be replaced"
    );
    assert!(String::from_utf8(o.stdout).is_err(), "the fixture must really be non-UTF-8");
}

/// The headline regression. The pre-extraction executor polled `try_wait()`
/// without draining, so a child that filled the pipe blocked on write, never
/// exited, and the deadline fired — reporting a *timeout* for what was only a
/// large result. Measured on that implementation with a 3s budget: 64 KiB
/// completed, 128 KiB reported a timeout.
///
/// 4 MiB is far past any platform's pipe buffer (64 KiB on Linux, 64 KiB on
/// macOS), so this fails loudly against the old behaviour and passes only when
/// the pipes are genuinely drained concurrently.
#[test]
fn output_far_larger_than_a_pipe_buffer_completes_instead_of_looking_like_a_hang() {
    const SIZE: usize = 4 * 1024 * 1024;
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", &format!("head -c {SIZE} /dev/zero | tr '\\0' 'x'")]);

    let c = run_bounded(cmd, Duration::from_secs(30)).expect("sh should spawn");
    match c {
        Completion::Exited(o) => {
            assert!(o.status.success());
            assert_eq!(
                o.stdout.len(),
                SIZE,
                "every byte must be collected, not just the first pipe-full"
            );
        }
        Completion::TimedOut { stdout, .. } => panic!(
            "4 MiB of output was reported as a TIMEOUT after collecting {} bytes — \
             the pipe-buffer deadlock is back",
            stdout.len()
        ),
    }
}

#[test]
fn a_large_payload_on_stderr_also_completes() {
    // Both pipes need their own reader; draining only stdout leaves the same
    // deadlock one redirect away.
    const SIZE: usize = 1024 * 1024;
    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        &format!("head -c {SIZE} /dev/zero | tr '\\0' 'y' >&2"),
    ]);
    let c = run_bounded(cmd, Duration::from_secs(30)).expect("sh should spawn");
    let Completion::Exited(o) = c else {
        panic!("1 MiB on stderr must not read as a timeout")
    };
    assert_eq!(o.stderr.len(), SIZE);
}

#[test]
fn both_pipes_filling_at_once_completes() {
    const SIZE: usize = 512 * 1024;
    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        &format!("head -c {SIZE} /dev/zero | tr '\\0' 'o' & head -c {SIZE} /dev/zero | tr '\\0' 'e' >&2; wait"),
    ]);
    let c = run_bounded(cmd, Duration::from_secs(30)).expect("sh should spawn");
    let Completion::Exited(o) = c else {
        panic!("concurrent pipe pressure must not read as a timeout")
    };
    assert_eq!(o.stdout.len(), SIZE);
    assert_eq!(o.stderr.len(), SIZE);
}

#[test]
fn a_hung_command_times_out_promptly() {
    let mut cmd = Command::new("/bin/sleep");
    cmd.arg("30");
    let start = Instant::now();
    let c = run_bounded(cmd, Duration::from_millis(300)).expect("sleep should spawn");
    let elapsed = start.elapsed();
    assert!(matches!(c, Completion::TimedOut { .. }), "must report a timeout, got {c:?}");
    assert!(
        elapsed < Duration::from_secs(5),
        "kill-on-timeout should be prompt, took {elapsed:?}"
    );
}

#[test]
fn a_timeout_preserves_output_emitted_before_the_deadline() {
    // Discarding it (the old `Ok(None)`) throws away the diagnostic that
    // explains why the thing hung.
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "echo progress-so-far; sleep 30"]);
    let c = run_bounded(cmd, Duration::from_millis(400)).expect("sh should spawn");
    match c {
        Completion::TimedOut { stdout, .. } => {
            assert_eq!(stdout, b"progress-so-far\n", "pre-deadline output must survive the kill");
        }
        Completion::Exited(_) => panic!("sleep 30 cannot finish in 400ms"),
    }
}

/// The second regression. `Child::kill()` reaches only the immediate child, so
/// a process that had forked left descendants running past the deadline —
/// verified by pid rather than by process name, because a name-based count
/// silently matches the measuring command itself.
#[test]
#[cfg(unix)]
fn a_timeout_terminates_descendants_not_just_the_immediate_child() {
    let dir = std::env::temp_dir().join(format!("loom-proc-exec-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let pidfile = dir.join("grandchild.pid");
    let _ = std::fs::remove_file(&pidfile);

    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        &format!("/bin/sleep 120 & echo $! > {}; exec /bin/sleep 120", pidfile.display()),
    ]);
    let c = run_bounded(cmd, Duration::from_millis(600)).expect("sh should spawn");
    assert!(matches!(c, Completion::TimedOut { .. }), "expected a timeout, got {c:?}");

    // Give the group kill a moment to be reaped before asking.
    std::thread::sleep(Duration::from_millis(300));

    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("grandchild must have recorded its pid")
        .trim()
        .parse()
        .expect("pid must parse");
    assert!(pid > 0, "fixture must capture a real pid");

    // SAFETY: signal 0 performs error checking only; it never delivers a signal.
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    if alive {
        // Do not leak the process if the assertion is about to fail.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        !alive,
        "grandchild pid {pid} outlived the deadline — the timeout killed only the immediate child"
    );
}

#[test]
fn succeeded_reports_false_for_a_timeout() {
    // A timeout must never read as success anywhere, including the convenience
    // accessors — that is the "never clean, never up to date" rule in miniature.
    let mut cmd = Command::new("/bin/sleep");
    cmd.arg("30");
    let c = run_bounded(cmd, Duration::from_millis(200)).expect("sleep should spawn");
    assert!(!c.succeeded());
    assert!(c.output().is_none(), "a timeout has no Output to hand back");
}

#[test]
fn succeeded_reports_false_for_a_nonzero_exit() {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "exit 1"]);
    let c = run_bounded(cmd, GENEROUS).expect("sh should spawn");
    assert!(!c.succeeded());
    assert!(c.output().is_some(), "a completed execution still yields its Output");
}
