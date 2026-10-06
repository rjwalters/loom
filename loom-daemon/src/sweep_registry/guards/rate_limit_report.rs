//! Dispatch-path forge writes feed the rate-limit breaker (Issue #8997 gap d).
//!
//! The `loom:building` label flip and the lease comment are often the very
//! first calls to meet an exhausted installation (2026-10-02: they failed on
//! every host minutes before any polling loop noticed), yet they only logged.
//! Reporting them here trips the shared breaker on the first authoritative
//! failure, so the work finder's existing suppression gate stops further
//! dispatches into the same wall. The current attempt is not changed: the
//! flip still returns its error (dispatch continues, as before) and the lease
//! comment stays fail-open — no new partial-claim path.

use crate::rate_limit_breaker::report::{BreakerHandle, FailureContext, ProbeMode, Reported};
use crate::sweep_registry::SweepRegistry;

impl SweepRegistry {
    /// Report a non-zero `gh` exit's `stderr` from a dispatch-path write.
    /// Ordinary failures (auth, not-found, network) are ignored here and keep
    /// their existing handling; a rate-limit signature trips the injected
    /// breaker (or the process-global one), probing in the background under
    /// this registry's workspace root and `gh` program.
    pub(in crate::sweep_registry) fn report_forge_failure(
        &self,
        stderr: &str,
        source: &str,
    ) -> Option<Reported> {
        if !crate::rate_limit_breaker::indicates_rate_limit(stderr) {
            return None;
        }
        let handle = self
            .config
            .rate_limit
            .clone()
            .or_else(|| BreakerHandle::global(ProbeMode::Background))?;
        let program = self.resolved_gh().to_string_lossy().into_owned();
        handle.report(
            stderr,
            source,
            FailureContext::for_root(&self.config.workspace_root, program),
        )
    }
}
