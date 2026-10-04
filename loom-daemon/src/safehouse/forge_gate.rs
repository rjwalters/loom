//! Rate-limit breaker gate for the sink's forge lookups (Issue #8997 gap c).
//!
//! Every safehouse `gh` lookup — dispatch-title enrichment, merge
//! verification, repo identity, and the periodic merge reconciliation —
//! consults the shared breaker at the lookup boundary ([`suppressed`]) and
//! reports a genuine rate-limit failure to it ([`failed`]). While cooling the
//! sink makes **zero** forge calls and keeps narrating with the data it has
//! (an un-titled dispatch line, an `ack` without its completion); a
//! completion it could not verify is *not* recorded as narrated, so the
//! reconciliation pass discovers it once the breaker releases.
//!
//! Reporting is separate from the deduplicated WARN logging: a repeat
//! failure that only logs at debug is still classified. The report uses
//! [`ProbeMode::Background`], so the async event loop never waits on the
//! budget probe.

use std::path::Path;

use crate::rate_limit_breaker::report::{BreakerHandle, FailureContext, ProbeMode};

tokio::task_local! {
    /// Test seam: the breaker for the current sink task. `None` inside a
    /// scope means "no breaker"; outside any scope the process-global
    /// breaker is used (unset ⇒ no gating, the pre-#8997 behavior).
    static GATE: Option<BreakerHandle>;
}

fn current() -> Option<BreakerHandle> {
    GATE.try_with(Clone::clone)
        .unwrap_or_else(|_| BreakerHandle::global(ProbeMode::Background))
}

/// Run `fut` with `gate` as the sink's breaker (tests inject one here
/// instead of registering the process-global singleton).
#[cfg(test)]
pub(super) async fn scoped<F: std::future::Future>(
    gate: Option<BreakerHandle>,
    fut: F,
) -> F::Output {
    GATE.scope(gate, fut).await
}

/// Whether the lookup `call` in `root` must be skipped: the shared API
/// budget is exhausted and the breaker is cooling.
pub(super) fn suppressed(call: &str, root: &Path) -> bool {
    let cooling = current().is_some_and(|gate| gate.is_suppressed());
    if cooling {
        log::debug!(
            "safehouse: skipping `gh {call}` in {} — rate-limit breaker cooling (#8997)",
            root.display()
        );
    }
    cooling
}

/// A lookup exited non-zero: report a rate-limit failure (with the failing
/// call's root and program) to the breaker, then log once per
/// `(call, workspace)` exactly as before.
pub(super) fn failed(call: &str, root: &Path, gh_bin: &str, stderr: &[u8]) {
    let text = String::from_utf8_lossy(stderr);
    if crate::rate_limit_breaker::indicates_rate_limit(&text) {
        if let Some(gate) = current() {
            let _ = gate.report(
                &text,
                &format!("safehouse ({call})"),
                FailureContext::for_root(root, gh_bin),
            );
        }
    }
    super::log_gh_failure_once(call, root, &super::stderr_head(stderr));
}
