use std::io::Write;

mod daemon_service;
// The lib's test-only env guard, compiled into this bin's test build too.
#[cfg(test)]
#[path = "env_test_support.rs"]
mod env_test_support;

/// Process entry point.
///
/// The real body lives in [`run_daemon`]; this wrapper exists so a startup
/// failure **terminates the process itself** instead of returning `Err` out of
/// `#[tokio::main]` (#4531).
///
/// Returning `Err` is correct but not prompt: the `#[tokio::main]` wrapper drops
/// its `Runtime` when `block_on` returns, and `Runtime::drop` blocks the calling
/// thread until every in-flight `spawn_blocking` task finishes. By the time the
/// singleton guard refuses to start (`IpcServer::run`, #3806), the daemon has
/// already spawned its periodic loops, several of which do their work on the
/// blocking pool — so the refusal message printed, then the process sat in
/// `Runtime::drop` for ~10s before `Termination` ever ran. Under a shorter
/// timeout that is indistinguishable from a hang, and it left a doomed second
/// daemon alive (running role-runner/work-finder ticks) long after it had
/// decided not to start.
///
/// The observable contract is deliberately unchanged: the message is the same
/// `Error: {err:?}` line `Termination for Result` would have written to stderr,
/// and [`loom_daemon::ipc::EXIT_STARTUP_FAILURE`] is `1`, the value
/// `ExitCode::FAILURE` carries.
/// Only the latency changes. stderr is explicitly flushed before
/// `std::process::exit` so the refusal is never truncated (`exit` runs no
/// destructors); the log file needs no such care because `env_logger` flushes
/// each record as it writes it — the same assumption every other
/// `std::process::exit` path in this daemon already makes.
#[tokio::main]
async fn main() {
    if let Err(err) = daemon_service::run_daemon().await {
        eprintln!("Error: {err:?}");
        let _ = std::io::stderr().flush();
        std::process::exit(loom_daemon::ipc::EXIT_STARTUP_FAILURE);
    }
}
