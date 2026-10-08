//! Build the `DaemonStatus` report off the async runtime (Issue #10765).
//!
//! [`super::build_daemon_status_with_drain`] is synchronous and `O(registered
//! roots)`: a registry load, per-root config/filesystem reads, the
//! `role_shard::decide` walk and a `.loom/locks/` scan per root. On a busy
//! dispatcher it takes 10s to over 100s (#8163's `slow build` WARN names the
//! phases). Before #10765 the build ran inline in the per-connection task, so
//! each in-progress `status` / `health` call held one tokio **worker** for its
//! whole duration. With as many concurrent status callers as workers
//! (fleet-check, `fleet-versions.py`, `health`, an operator's `status`), every
//! worker was pinned and unrelated light requests on the same socket — the
//! watchdog's `quarantine list` probe among them — missed their budget while
//! the daemon was alive.
//!
//! [`daemon_status_response`] runs the build on the blocking pool instead, so
//! a slow build only ever costs a blocking-pool thread and the workers stay
//! free for the rest of the IPC surface.
//!
//! The #4279 guarantee is kept: a panic inside the build still produces an
//! explicit error frame, never a dropped socket with zero bytes written.

use std::path::Path;
use std::sync::Arc;

use super::DrainState;
use crate::main_health_gate::WorkspaceHealthStates;
use crate::status_section::SectionSet;
use crate::types::{CredentialPreflightReport, DaemonStatusReport, Response};
use crate::workspace_pool::WorkspacePool;

/// Test-only: milliseconds every status build sleeps first, so a test can
/// hold builds in flight and probe the rest of the IPC surface meanwhile.
#[cfg(test)]
pub(super) static TEST_BUILD_DELAY_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The `DaemonStatus` / `DaemonStatusSections` reply for the IPC handler: the
/// CPU-sample pre-warm (#4031) and the report build, both on the blocking
/// pool. `sections` scopes the build (#10787); the pre-warm runs only when
/// `dynamic_cap` (the one section reporting the sample) is requested.
pub(super) async fn serve(
    workspace_pool: &Arc<WorkspacePool>,
    health_states: &Arc<WorkspaceHealthStates>,
    fallback_root: &Path,
    credential_preflight: &Arc<CredentialPreflightReport>,
    drain_state: &Arc<DrainState>,
    sections: SectionSet,
) -> Response {
    let (pool, health, credentials, drain) = (
        workspace_pool.clone(),
        health_states.clone(),
        credential_preflight.clone(),
        drain_state.clone(),
    );
    let root = fallback_root.to_path_buf();
    daemon_status_response(move || {
        // The macOS `iostat` read sleeps ~1s (#4031). A panic in it is not a
        // status failure: the build falls back to the last cached sample.
        if sections.needs_cpu_sample() {
            let _ = std::panic::catch_unwind(crate::cpu_headroom::refresh_cpu_util_cache);
        }
        #[cfg(test)]
        std::thread::sleep(std::time::Duration::from_millis(
            TEST_BUILD_DELAY_MS.load(std::sync::atomic::Ordering::SeqCst),
        ));
        super::build_daemon_status_with_drain(
            &pool,
            &health,
            &root,
            &credentials,
            &drain,
            &sections,
        )
    })
    .await
}

/// Run `build` on the blocking pool and turn its outcome into the
/// `DaemonStatus` reply frame.
///
/// * The report → `Response::DaemonStatus` (boxed, #4292).
/// * A panic inside `build` → `Response::Error` naming the panic cause
///   (#4279), logged at ERROR. The panic is caught inside the blocking task,
///   so no unwinding crosses the `.await`.
/// * A join error (the runtime shutting down underneath the task) → also an
///   error frame, for the same reason: the client must get a frame.
pub(super) async fn daemon_status_response<F>(build: F) -> Response
where
    F: FnOnce() -> DaemonStatusReport + Send + 'static,
{
    let joined = tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(build))
    })
    .await;
    let cause = match joined {
        // `Response::DaemonStatus` is boxed (issue #4292) to keep the enum small.
        Ok(Ok(report)) => return Response::DaemonStatus(Box::new(report)),
        Ok(Err(panic)) => super::describe_panic(panic.as_ref()),
        Err(join_err) => format!("status build task did not complete: {join_err}"),
    };
    log::error!(
        "DaemonStatus handler failed while building the report: {cause}; \
         replying with an error frame instead of dropping the connection"
    );
    Response::Error {
        message: format!("daemon failed to build status report: {cause}"),
    }
}

#[cfg(test)]
mod tests;
