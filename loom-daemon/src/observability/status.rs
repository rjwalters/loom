//! Delivery accounting shared by native HTTPS and OTLP senders.
use super::exporter;

/// Live record of whether telemetry is actually *reaching* the backend, written
/// by [`super::sender::try_flush`] on every attempt and read back by
/// [`super::global_export_status`] for `loom-daemon status` / `health`.
///
/// The complement to [`super::HostIdStatus`], which is anomaly-only by design (#4830)
/// and therefore cannot answer "is it working?" — only "is it working *wrong*
/// in this one specific way?". This cell is always readable and always has an
/// answer, including the answer #4830 could never give: *configured, running,
/// and has never successfully exported anything*.
///
/// Unlike [`super::HostIdStatus`] this is **not** write-once: it is a rolling record,
/// so `last_success_at` genuinely means "the last time data landed" and a watch
/// loop can alert on its age.
#[derive(Debug)]
pub struct ExportStatus {
    inner: std::sync::Mutex<crate::types::ObservabilityExportStatus>,
}

// Allow expect_used: a poisoned status mutex means another thread panicked
// while holding it — unrecoverable, matching the crash-on-poison policy
// `HostIdStatus` above (and `auto_update`/`ipc`) already use.
#[allow(clippy::expect_used)]
impl ExportStatus {
    /// A status cell for an exporter that is starting *now* under `host_id`,
    /// pushing to `endpoint` via `exporter` every `flush_interval_secs`.
    #[must_use]
    pub fn started(
        host_id: &str,
        endpoint: &str,
        exporter: &str,
        flush_interval_secs: u64,
    ) -> Self {
        let mut initial = crate::types::ObservabilityExportStatus {
            state: crate::types::ObservabilityExportState::Starting,
            host_id: Some(host_id.to_string()),
            ingest_host_id: None,
            endpoint: Some(endpoint.to_string()),
            exporter: Some(exporter.to_string()),
            started_at: Some(chrono::Utc::now()),
            last_success_at: None,
            last_failure_at: None,
            last_failure_detail: None,
            records_exported: 0,
            signal_counts: Default::default(),
            consecutive_failures: 0,
            flush_interval_secs: Some(flush_interval_secs),
            ..Default::default()
        };
        // #9015: stamp the first-hop scope and whether this endpoint is a local
        // collector, from the endpoint just recorded above.
        initial.refresh_endpoint_scope();
        ExportStatus {
            inner: std::sync::Mutex::new(initial),
        }
    }

    /// Record `count` envelopes acked by the backend. Clears the consecutive-
    /// failure run: the transport demonstrably works again.
    pub fn record_success(&self, count: usize) {
        let mut guard = self
            .inner
            .lock()
            .expect("observability export status mutex poisoned");
        guard.last_success_at = Some(chrono::Utc::now());
        guard.records_exported = guard
            .records_exported
            .saturating_add(count.try_into().unwrap_or(u64::MAX));
        guard.consecutive_failures = 0;
    }

    /// Counts use signal units, never envelope counts (one envelope may contain many points).
    pub fn record_signals(
        &self,
        counts: &std::collections::BTreeMap<String, exporter::SignalCounts>,
    ) {
        let mut guard = self
            .inner
            .lock()
            .expect("observability export status mutex poisoned");
        for (signal, delta) in counts {
            guard
                .signal_counts
                .entry(signal.clone())
                .or_default()
                .accumulate(delta);
        }
    }

    /// Record a failed flush attempt. `detail` is the exporter's own error
    /// text; the previous `last_success_at` is deliberately preserved so the
    /// surfaces can distinguish "used to work, broke 30s ago" from "never
    /// worked at all".
    pub fn record_failure(&self, detail: &str) {
        let mut guard = self
            .inner
            .lock()
            .expect("observability export status mutex poisoned");
        guard.last_failure_at = Some(chrono::Utc::now());
        guard.last_failure_detail = Some(detail.to_string());
        guard.consecutive_failures = guard.consecutive_failures.saturating_add(1);
    }

    /// A status cell for an exporter that never started because a required
    /// piece of config could not be resolved (Issue #5337) — `enabled: true`
    /// but no endpoint, no ingest key file, or an unreadable/empty ingest key
    /// file. See [`crate::types::ObservabilityExportStatus::misconfigured`]
    /// for the field semantics.
    #[must_use]
    pub fn misconfigured(endpoint: Option<String>, detail: String) -> Self {
        ExportStatus {
            inner: std::sync::Mutex::new(crate::types::ObservabilityExportStatus::misconfigured(
                endpoint, detail,
            )),
        }
    }

    /// The current record, with [`crate::types::ObservabilityExportStatus::state`]
    /// (and the #9015 scope fields) re-derived as of now. `ingest_host_id` is folded in from
    /// [`super::global_host_id_mismatch`] by [`super::global_export_status`], not here — this
    /// cell knows nothing about identity.
    #[must_use]
    pub fn snapshot(&self) -> crate::types::ObservabilityExportStatus {
        let mut snapshot = self
            .inner
            .lock()
            .expect("observability export status mutex poisoned")
            .clone();
        snapshot.state = snapshot.classify(chrono::Utc::now());
        snapshot.refresh_endpoint_scope();
        snapshot
    }
}
