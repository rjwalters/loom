//! SIGUSR1 in-process async task dump for a hung daemon tick (#12079).
//!
//! Hosts restrict ptrace (yama `ptrace_scope=1`), so a wedged tick cannot be
//! inspected with gdb/strace without root. Instead the daemon listens for
//! SIGUSR1 and writes tokio's async task tree to its own log, as the daemon's
//! own user, without stopping it. The listener does not depend on the IPC
//! accept loop, which may itself be wedged, and runs on its own OS thread with
//! its own tiny runtime: a stalled main runtime cannot fire the dump's timeout,
//! so the timer must live elsewhere.
//!
//! The dump needs tokio's `taskdump` feature (`Cargo.toml`) plus
//! `--cfg tokio_unstable` (`.cargo/config.toml`) on Linux x86/x86_64/aarch64.
//! Any other build still installs
//! the handler — SIGUSR1's default action would otherwise kill the daemon — and
//! logs a single "task dump unavailable" line per signal.

use std::time::Duration;
use tokio::runtime::Handle;

/// Upper bound on one dump: a task that never yields can stall tokio's dump.
pub const DUMP_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether this build can produce a task dump.
pub const DUMP_AVAILABLE: bool = cfg!(all(
    tokio_unstable,
    target_os = "linux",
    any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")
));

/// Result of one dump attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum DumpOutcome {
    /// One entry per task: its rendered trace.
    Tasks(Vec<String>),
    TimedOut,
    Unavailable,
}

#[cfg(all(
    tokio_unstable,
    target_os = "linux",
    any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")
))]
async fn collect(handle: &Handle, timeout: Duration) -> DumpOutcome {
    match tokio::time::timeout(timeout, handle.dump()).await {
        Ok(dump) => {
            DumpOutcome::Tasks(dump.tasks().iter().map(|t| t.trace().to_string()).collect())
        }
        Err(_) => DumpOutcome::TimedOut,
    }
}

#[cfg(not(all(
    tokio_unstable,
    target_os = "linux",
    any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")
)))]
async fn collect(_handle: &Handle, _timeout: Duration) -> DumpOutcome {
    DumpOutcome::Unavailable
}

/// Take one dump of `handle`'s runtime and write it to the daemon log. Await
/// this from a runtime other than `handle`'s, or the timeout cannot fire when
/// `handle`'s runtime is the stalled one.
pub async fn dump_to_log(handle: &Handle, timeout: Duration) -> DumpOutcome {
    let outcome = collect(handle, timeout).await;
    match &outcome {
        DumpOutcome::Tasks(tasks) => {
            log::info!("SIGUSR1 task dump: {} task(s)", tasks.len());
            for (i, trace) in tasks.iter().enumerate() {
                log::info!("task dump [{}/{}]:\n{trace}", i + 1, tasks.len());
            }
        }
        DumpOutcome::TimedOut => log::warn!(
            "SIGUSR1 task dump timed out after {}s (a task is not yielding to the runtime)",
            timeout.as_secs()
        ),
        DumpOutcome::Unavailable => log::info!(
            "SIGUSR1 task dump unavailable: this build lacks `--cfg tokio_unstable` or is not \
             Linux x86/x86_64/aarch64"
        ),
    }
    outcome
}

/// Spawn the SIGUSR1 listener for the current runtime on a dedicated thread.
/// Each signal produces one dump; the daemon keeps running. Must be called
/// from within the runtime to inspect. A no-op on non-Unix platforms.
pub fn spawn_signal_listener() {
    #[cfg(unix)]
    {
        let handle = Handle::current();
        let spawned = std::thread::Builder::new()
            .name("task-dump".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(err) => return log::warn!("SIGUSR1 task-dump runtime failed: {err}"),
                };
                rt.block_on(async move {
                    use tokio::signal::unix::{signal, SignalKind};
                    let mut stream = match signal(SignalKind::user_defined1()) {
                        Ok(stream) => stream,
                        Err(err) => {
                            return log::warn!("Unable to install SIGUSR1 task-dump handler: {err}")
                        }
                    };
                    while stream.recv().await.is_some() {
                        dump_to_log(&handle, DUMP_TIMEOUT).await;
                    }
                });
            });
        if let Err(err) = spawned {
            log::warn!("Unable to start SIGUSR1 task-dump thread: {err}");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn side_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn sigusr1_does_not_kill_the_process_and_listener_survives_repeats() {
        let main_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        main_rt.block_on(async { spawn_signal_listener() });
        // Give the listener thread time to install its handler before raising,
        // or the default action would terminate the test process.
        std::thread::sleep(Duration::from_millis(300));
        for _ in 0..2 {
            // SAFETY: raising a signal whose handler is installed above.
            assert_eq!(unsafe { libc::raise(libc::SIGUSR1) }, 0);
            std::thread::sleep(Duration::from_millis(200));
        }
        // Reaching here means the process survived both signals.
        let outcome = side_runtime().block_on(dump_to_log(main_rt.handle(), DUMP_TIMEOUT));
        assert!(!matches!(outcome, DumpOutcome::TimedOut), "dump timed out on an idle runtime");
        assert_eq!(matches!(outcome, DumpOutcome::Tasks(_)), DUMP_AVAILABLE);
    }

    #[cfg(all(
        tokio_unstable,
        target_os = "linux",
        any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn dump_includes_a_parked_task() {
        let main_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        main_rt.spawn(std::future::pending::<()>());
        std::thread::sleep(Duration::from_millis(100));
        match side_runtime().block_on(dump_to_log(main_rt.handle(), DUMP_TIMEOUT)) {
            DumpOutcome::Tasks(tasks) => assert!(!tasks.is_empty()),
            other => panic!("expected tasks, got {other:?}"),
        }
    }

    #[cfg(all(
        tokio_unstable,
        target_os = "linux",
        any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn stalled_dump_times_out() {
        // The only worker is blocked by a task that never yields, so the dump
        // cannot complete; the side runtime's timeout must still fire.
        let main_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        main_rt.spawn(async { std::thread::sleep(Duration::from_secs(3)) });
        std::thread::sleep(Duration::from_millis(200));
        let outcome =
            side_runtime().block_on(dump_to_log(main_rt.handle(), Duration::from_millis(300)));
        assert_eq!(outcome, DumpOutcome::TimedOut);
    }
}
