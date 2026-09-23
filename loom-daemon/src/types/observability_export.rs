//! Observable transport delivery status and per-signal units.
use super::ObservabilityExportState;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A confirmed disagreement between the host identity this daemon resolves for
/// itself and the `host_id` the ingest backend echoes back for the key it
/// authenticated (Issue #4830).
///
/// Filed as a *data* type on the status wire rather than a log-only condition
/// because the 2026-07-31 incident it exists for was invisible for hours: a Mac
/// Studio pushed its whole first night of telemetry under another host's id
/// because the wrong key file had been installed on it, and neither side had any
/// way to notice. The backend cannot notice (a key-bound id is authoritative by
/// design), so the *daemon* is the only party that holds both halves.
///
/// Lives beside [`ObservabilityExportStatus`] in this sibling module (moved
/// from `types.rs` inline for the file-size ratchet, #8756); re-exported as
/// `crate::types::ObservabilityHostIdMismatch` so every existing path is
/// unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObservabilityHostIdMismatch {
    /// What this daemon calls itself —
    /// [`crate::sweep_registry::host_identity`], resolved with the precedence
    /// `$LOOM_HOST_ID`, then `$HOSTNAME`, then the `hostname` binary, then
    /// `"unknown-host"`. The same value it stamps on every outgoing envelope.
    pub daemon_host_id: String,
    /// The `host_id` the `/ingest` response echoed — the identity the
    /// authenticated key is bound to, i.e. the host every pushed record is
    /// actually being filed under.
    pub ingest_host_id: String,
    /// When the mismatch was first observed this daemon process. Never
    /// re-stamped on subsequent flushes: the WARN and this record are both
    /// once-per-lifetime, so this is the age of the condition, not of the last
    /// flush.
    pub first_seen_at: DateTime<Utc>,
}

/// Positive, always-present state of this daemon's telemetry export (Issue
/// #5083) — the counterpart to [`ObservabilityHostIdMismatch`]'s anomaly-only
/// signal.
///
/// The 2026-08-03 incident this exists for: two hosts with byte-identical
/// observability config, one showing an `observability` health section and one
/// not. The absence was the *only* evidence the second host was fine, which is
/// inference from absence — and the exact same absence would have been shown
/// for a host whose exporter had never successfully sent a single batch.
/// Confirming it took a `daemon.log` grep for the *lack* of a warning.
///
/// Published by [`crate::observability::ExportStatus`] (updated by
/// [`crate::observability::sender::try_flush`] on every attempt) and read back
/// via [`crate::observability::global_export_status`], mirroring the
/// process-global pattern [`ObservabilityHostIdMismatch`] already uses.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObservabilityExportStatus {
    /// The state as classified by the *daemon* at status-build time. Consumers
    /// that hold their own `now` (e.g. `loom-daemon health`, which stamps one
    /// `at` for the whole report) may re-derive it with [`Self::classify`];
    /// both agree except across the grace boundary.
    ///
    /// `#[serde(default)]` (⇒ `disabled`) so a partial payload from any other
    /// producer still parses rather than failing the whole status read — every
    /// consumer that cares re-derives with [`Self::classify`] anyway.
    #[serde(default)]
    pub state: ObservabilityExportState,
    /// The host identity this daemon stamps on every outgoing envelope —
    /// [`crate::sweep_registry::host_identity`]. `None` when the exporter is
    /// not running. This is the "under which `host_id`" half of the AC.
    #[serde(default)]
    pub host_id: Option<String>,
    /// The `host_id` the ingest backend echoed back, when it *disagrees* with
    /// [`Self::host_id`] — i.e. `Some` exactly when a #4830 mismatch has been
    /// confirmed, so [`Self::classify`] needs no second input. `None`
    /// otherwise, including when the ids agree.
    #[serde(default)]
    pub ingest_host_id: Option<String>,
    /// The configured export endpoint, so an operator can confirm *where* the
    /// data is going without opening the config. `None` when not running.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Which exporter implementation is running: `"https"` (Loom's native
    /// ingest) or `"otlp"`. `None` when not running.
    #[serde(default)]
    pub exporter: Option<String>,
    /// When the exporter task started this daemon process. The denominator for
    /// "has it had a fair chance to flush yet" — see [`Self::classify`].
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    /// When a batch was most recently acked by the backend. `None` means *no
    /// batch has ever been acked this process* — the never-exported signal.
    #[serde(default)]
    pub last_success_at: Option<DateTime<Utc>>,
    /// When a flush attempt most recently failed, if ever.
    #[serde(default)]
    pub last_failure_at: Option<DateTime<Utc>>,
    /// The error text of that most recent failure (the exporter's own
    /// `Display`, e.g. `sink rejected batch: HTTP 401 — …`). Never contains
    /// the ingest key: the exporter's errors are built from status codes and
    /// truncated body snippets only.
    ///
    /// Also doubles as the [`ObservabilityExportState::Misconfigured`] detail
    /// (#5337) — the offending config path plus the underlying error (e.g. a
    /// missing-file `io::Error`'s `Display`, which includes the OS errno on
    /// platforms that report one). Same "never the key itself" discipline.
    #[serde(default)]
    pub last_failure_detail: Option<String>,
    /// Total envelopes acked this daemon process. `0` alongside a `Some`
    /// `started_at` is the never-exported signature.
    #[serde(default)]
    pub records_exported: u64,
    /// OTLP counters keyed by `log_records`, `metric_data_points`, or `spans`.
    #[serde(default)]
    pub signal_counts:
        std::collections::BTreeMap<String, crate::observability::outcome::SignalCounts>,
    /// Consecutive failed flush attempts since the last success (reset to 0 on
    /// any ack). Non-zero ⇒ [`ObservabilityExportState::Failing`].
    #[serde(default)]
    pub consecutive_failures: u32,
    /// The flush cadence this exporter resolved, in seconds — the unit the
    /// grace window is scaled by, so a host configured with a long interval
    /// does not false-alarm as never-exported. `None` when not running.
    #[serde(default)]
    pub flush_interval_secs: Option<u64>,
}

/// Floor on the never-exported grace window — a fresh exporter is never called
/// out before this much wall-clock has passed, regardless of flush cadence.
/// Sized above the collector's own 5-minute host-snapshot interval
/// ([`crate::observability::SNAPSHOT_INTERVAL`]) so a host with no sweep
/// activity at all still has had at least one record enqueued and one flush
/// attempted before the window closes.
pub const NEVER_EXPORTED_GRACE_FLOOR_SECS: u64 = 10 * 60;

impl ObservabilityExportStatus {
    /// The "deliberately off" reading — `observability.enabled` is `false` (or
    /// the block is absent). Distinct from a `None` field, which means "this
    /// daemon binary predates #5083 and cannot tell you", **and** distinct
    /// from [`Self::misconfigured`] (#5337) — `enabled: true` with a config
    /// problem is never reported this way.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            state: ObservabilityExportState::Disabled,
            ..Self::default()
        }
    }

    /// The "enabled but a required piece of config is missing or unusable"
    /// reading (#5337) — `observability.enabled` is `true` but the exporter
    /// never started because `endpoint`/`ingestKeyFile` did not resolve, or
    /// the ingest key file could not be read. `endpoint` carries whatever
    /// *did* resolve (`None` only when the endpoint itself is what's missing)
    /// so an operator can see where telemetry would have gone; `detail` names
    /// the offending path and the underlying error (never the key itself).
    #[must_use]
    pub fn misconfigured(endpoint: Option<String>, detail: String) -> Self {
        Self {
            state: ObservabilityExportState::Misconfigured,
            endpoint,
            last_failure_detail: Some(detail),
            ..Self::default()
        }
    }

    /// How long a freshly-started exporter is given before a still-empty
    /// success record is called out: three flush intervals, floored at
    /// [`NEVER_EXPORTED_GRACE_FLOOR_SECS`].
    #[must_use]
    pub fn never_exported_grace_secs(&self) -> u64 {
        self.flush_interval_secs
            .unwrap_or(0)
            .saturating_mul(3)
            .max(NEVER_EXPORTED_GRACE_FLOOR_SECS)
    }

    /// Seconds since the last acked batch, as of `now`. `None` when nothing has
    /// ever been acked. Clamped at zero so a small clock skew never renders a
    /// negative age.
    #[must_use]
    pub fn last_success_age_secs(&self, now: DateTime<Utc>) -> Option<u64> {
        self.last_success_at.map(|at| {
            u64::try_from(now.signed_duration_since(at).num_seconds().max(0)).unwrap_or(0)
        })
    }

    /// Seconds the exporter has been running, as of `now`. `None` when it is
    /// not running.
    #[must_use]
    pub fn uptime_secs(&self, now: DateTime<Utc>) -> Option<u64> {
        self.started_at.map(|at| {
            u64::try_from(now.signed_duration_since(at).num_seconds().max(0)).unwrap_or(0)
        })
    }

    /// Derive the state from the recorded facts, as of `now`. Pure, so every
    /// surface (`status`, `health`, the dashboard) reaches the same verdict
    /// from the same wire payload rather than each re-inventing the rules.
    ///
    /// Precedence, most-specific first:
    /// 0. explicitly recorded as misconfigured ⇒ `Misconfigured` (#5337) — the
    ///    exporter never started (no `started_at`), so this has to be checked
    ///    *before* the not-running fallback below or it would silently
    ///    collapse into `Disabled`, which is exactly the bug this precedence
    ///    branch exists to prevent. `self.state` is otherwise never an input
    ///    to this function (every other branch re-derives from the other
    ///    fields) — `Misconfigured` is the one sticky, explicitly-set
    ///    terminal state, since nothing else ever transitions out of it.
    /// 1. not running ⇒ `Disabled`
    /// 2. confirmed id disagreement ⇒ `HostIdMismatch` (config-shaped, cannot
    ///    self-recover — outranks a transient flush failure, whose facts stay
    ///    readable in `last_failure_*` either way)
    /// 3. the last attempt failed ⇒ `Failing`
    /// 4. something has been acked ⇒ `Healthy`
    /// 5. nothing acked, still inside the grace window ⇒ `Starting`
    /// 6. nothing acked, past the grace window ⇒ `NeverExported`
    #[must_use]
    pub fn classify(&self, now: DateTime<Utc>) -> ObservabilityExportState {
        if self.state == ObservabilityExportState::Misconfigured {
            return ObservabilityExportState::Misconfigured;
        }
        let Some(uptime) = self.uptime_secs(now) else {
            return ObservabilityExportState::Disabled;
        };
        if self
            .ingest_host_id
            .as_ref()
            .is_some_and(|ingest| Some(ingest) != self.host_id.as_ref())
        {
            return ObservabilityExportState::HostIdMismatch;
        }
        if self.consecutive_failures > 0 {
            return ObservabilityExportState::Failing;
        }
        if self.last_success_at.is_some() {
            return ObservabilityExportState::Healthy;
        }
        if uptime < self.never_exported_grace_secs() {
            ObservabilityExportState::Starting
        } else {
            ObservabilityExportState::NeverExported
        }
    }
}
