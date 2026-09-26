//! The conditional `observability` health section (#4830 / #5083 / #5337).
//!
//! Extracted from `health.rs` into its own file (#8572) for the same reason
//! [`super::calibration_section`], [`super::codesign`] and
//! [`super::transcript_ingest_section`] live here: `health.rs` is over the
//! file-size ratchet threshold (`.loom/docs/file-size-policy.md`) and may not
//! grow. Pure move — no behavior change; `health::assess_observability`
//! remains the public name via the re-export in `health.rs`.

use super::{format_window, HealthInputs, HealthSection, Verdict};
use crate::types::ObservabilityExportState;

/// Assess the observability exporter: `Some(DEGRADED)` when telemetry is
/// demonstrably going wrong, else `None`.
///
/// **Deliberately conditional**, unlike every other section. There is nothing
/// to say when the exporter is disabled, still starting, or exporting under the
/// right identity — which is all but a handful of daemons — so a permanent
/// `observability GREEN — ok` line would be pure noise on a surface whose whole
/// value is that every line printed is worth reading. That anomaly-only rule
/// (#4830) is preserved verbatim; the *positive* confirmation an operator needs
/// lives on `loom-daemon status` instead (`Observability: OK — …`, #5083) and,
/// machine-readably, in `DaemonStatusReport::observability_export`.
///
/// Four conditions qualify as anomalies:
///
/// 1. **host-identity mismatch** (#4830) — the daemon has confirmed its ingest
///    key is bound to a *different* `host_id` than it reports for itself, so
///    every record it pushes is filed under the wrong host. Kept first and
///    byte-for-byte as it was, including its `detail` keys.
/// 2. **never exported** (#5083) — the exporter has been running well past its
///    flush cadence and has still never had a single batch acked. This is the
///    silent failure this section previously rendered *identically to healthy*:
///    as nothing at all.
/// 3. **export failing** (#5083) — flushes are actively erroring, so the queue
///    is backing up and telemetry is going stale.
/// 4. **misconfigured** (#5337) — `enabled: true` but the exporter never
///    started because a required piece of config (endpoint, ingest key file,
///    or a readable/non-empty key) could not be resolved. Distinct from the
///    silent, no-section `Disabled` state below: this is a config error an
///    operator should fix, not a deliberate opt-out.
///
/// Read straight off [`DaemonStatusReport`] rather than through a dedicated
/// [`HealthInputs`] field: this is *daemon-process* state (only the daemon
/// holds both halves — its own identity and the backend's responses), and
/// `health` runs in a separate CLI process, so the IPC status report is the
/// only place it can come from. A parallel collector field would just copy it
/// and add a way for the two to disagree.
#[must_use]
pub fn assess_observability(inputs: &HealthInputs) -> Option<HealthSection> {
    let status = inputs.status.as_ref()?;
    // Positive facts, when the daemon is new enough to report them (#5083) —
    // folded into the mismatch note's `detail` below so a machine consumer
    // reading a DEGRADED section still learns whether anything is landing at
    // all, and used on its own for conditions 2 and 3.
    //
    // `state` is re-stamped from this report's own `at` before serialization:
    // the daemon classified it at status-build time, and a section whose
    // verdict said one thing while its `detail.state` said another would be a
    // new way for the two halves of the same answer to disagree — precisely
    // the failure mode this issue is about.
    let export = status.observability_export.as_ref().map(|e| {
        let mut classified = e.clone();
        classified.state = classified.classify(inputs.at);
        // #9015: re-derive the scope fields for the same reason the state is
        // re-stamped — a payload from an older daemon carries no
        // `scope`/`endpoint_loopback`, and a `detail` that omits how far the
        // state reaches is the misreading this issue is about.
        classified.refresh_endpoint_scope();
        classified
    });
    let export = export.as_ref();
    let export_detail = export.map_or(serde_json::Value::Null, |e| {
        serde_json::to_value(e).unwrap_or(serde_json::Value::Null)
    });

    if let Some(mismatch) = status.observability_host_id_mismatch.as_ref() {
        let age = inputs
            .at
            .signed_duration_since(mismatch.first_seen_at)
            .num_seconds()
            .max(0);
        return Some(HealthSection::new(
            "observability",
            Verdict::Degraded,
            format!(
                "telemetry is being filed under {} — the ingest key on this host is bound to that \
                 id, not to {} (first seen {} ago)",
                mismatch.ingest_host_id,
                mismatch.daemon_host_id,
                format_window(u64::try_from(age).unwrap_or(0))
            ),
            serde_json::json!({
                "daemon_host_id": mismatch.daemon_host_id,
                "ingest_host_id": mismatch.ingest_host_id,
                "first_seen_at": mismatch.first_seen_at,
                "first_seen_age_secs": age,
                "export": export_detail,
            }),
        ));
    }

    let export = export?;
    match export.classify(inputs.at) {
        // The #5083 headline: configured, running, and has never once
        // succeeded. Called out only after the grace window
        // (`never_exported_grace_secs`) so a freshly-restarted daemon is never
        // reported as broken for its first flush interval.
        ObservabilityExportState::NeverExported => {
            Some(HealthSection::new(
                "observability",
                Verdict::Degraded,
                format!(
                "exporter has been running {} as {} and has NEVER had a batch acked — telemetry \
                 is not reaching {}{}",
                format_window(export.uptime_secs(inputs.at).unwrap_or(0)),
                export.host_id.as_deref().unwrap_or("unknown-host"),
                export.endpoint.as_deref().unwrap_or("the configured endpoint"),
                export
                    .last_failure_detail
                    .as_deref()
                    .map_or_else(String::new, |d| format!(" (last error: {d})")),
            ),
                export_detail,
            ))
        }
        ObservabilityExportState::Failing => Some(HealthSection::new(
            "observability",
            Verdict::Degraded,
            format!(
                "{} consecutive failed flush(es) as {}; last successful export {}{}",
                export.consecutive_failures,
                export.host_id.as_deref().unwrap_or("unknown-host"),
                export.last_success_age_secs(inputs.at).map_or_else(
                    || "never".to_string(),
                    |age| format!("{} ago", format_window(age))
                ),
                export
                    .last_failure_detail
                    .as_deref()
                    .map_or_else(String::new, |d| format!(" (last error: {d})")),
            ),
            export_detail,
        )),
        // `enabled: true` but a required piece of config could not be
        // resolved (Issue #5337) — a real, operator-actionable config error,
        // not the benign `Disabled` steady state, so it earns a section same
        // as `NeverExported`/`Failing` above.
        ObservabilityExportState::Misconfigured => Some(HealthSection::new(
            "observability",
            Verdict::Degraded,
            format!(
                "enabled but not exporting — configuration is incomplete or unreadable{}",
                export
                    .last_failure_detail
                    .as_deref()
                    .map_or_else(String::new, |d| format!(": {d}")),
            ),
            export_detail,
        )),
        // Disabled / Starting / Healthy / HostIdMismatch (handled above) — no
        // section, exactly as before #5083.
        _ => None,
    }
}
