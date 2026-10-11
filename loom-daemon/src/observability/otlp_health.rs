//! OTLP export health (Issue #11353): "does this daemon export to SigNoz?" as
//! an explicit, four-valued health check instead of an absence.
//!
//! A daemon with no OTLP exporter, or one whose exports silently fail, looks
//! healthy everywhere else. This module reduces the question to one pure rule,
//! [`evaluate`], over plain inputs and an injected `now`, so tests never wait.
//! Precedence: `exempt`, then `no_exporter`, then `failing`, then `ok`.
//!
//! [`OtlpHealthMonitor`] holds the little state the rule needs across samples
//! (when `dropped_total` last grew, the last reported state) and tells its
//! caller when the state changed, so the WARN is logged once per change and not
//! once per tick. One process-global monitor serves the daemon start, the
//! collector tick and `status`; it is idempotent for repeated observations of
//! the same counters, so those callers cannot disturb one another.

use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::ObservabilityConfig;

/// Default failure window in minutes (`observability.otlp_failure_window_minutes`).
pub const DEFAULT_FAILURE_WINDOW_MINUTES: u64 = 15;

/// The exporter name the OTLP cell is keyed by in the status maps.
const OTLP_EXPORTER: &str = "otlp";

/// The four OTLP-export health states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OtlpExportState {
    /// No OTLP exporter will run: not configured, observability off, or the
    /// binary was built without the `otlp` feature (#10700).
    NoExporter,
    /// An OTLP exporter is configured but no batch succeeded within the
    /// window, or `dropped_total` is growing.
    Failing,
    /// The last successful export is recent.
    Ok,
    /// The operator opted out (`observability.otlp_required: false`) with a
    /// reason; the exclusion is deliberate and visible.
    Exempt,
}

impl OtlpExportState {
    /// The wire / display token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoExporter => "no_exporter",
            Self::Failing => "failing",
            Self::Ok => "ok",
            Self::Exempt => "exempt",
        }
    }
}

/// A state plus the human-readable detail (the exemption reason, or why the
/// exporter is absent or failing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OtlpExportHealth {
    pub state: OtlpExportState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Everything [`evaluate`] reads. Plain data so tests build it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtlpHealthInputs {
    /// `cfg!(feature = "otlp")` of the running binary.
    pub feature_otlp: bool,
    /// The resolved config plans at least one OTLP exporter
    /// ([`super::planned_otlp_exporters`] non-empty).
    pub otlp_planned: bool,
    /// Validated exemption reason; `Some` means the host is exempt.
    pub exempt_reason: Option<String>,
    /// When the OTLP exporter started (or, before it has, this monitor first
    /// looked): the start of the startup grace.
    pub started_at: Option<DateTime<Utc>>,
    /// Last acked OTLP batch.
    pub last_success_at: Option<DateTime<Utc>>,
    /// When `dropped_total` was last seen to grow.
    pub last_drop_growth_at: Option<DateTime<Utc>>,
    /// The failure window.
    pub window: Duration,
}

/// The pure rule. `now` is injected.
#[must_use]
pub fn evaluate(inputs: &OtlpHealthInputs, now: DateTime<Utc>) -> OtlpExportHealth {
    let health = |state, detail: Option<String>| OtlpExportHealth { state, detail };
    if let Some(reason) = &inputs.exempt_reason {
        return health(OtlpExportState::Exempt, Some(reason.clone()));
    }
    if !inputs.feature_otlp {
        return health(
            OtlpExportState::NoExporter,
            Some("this build has no `otlp` feature".to_string()),
        );
    }
    if !inputs.otlp_planned {
        return health(
            OtlpExportState::NoExporter,
            Some("no usable `otlp` entry in observability.exporters".to_string()),
        );
    }
    let minutes = inputs.window.num_minutes();
    if let Some(grew) = inputs.last_drop_growth_at {
        if now.signed_duration_since(grew) <= inputs.window {
            return health(
                OtlpExportState::Failing,
                Some(format!("export queue dropped records within the last {minutes}m")),
            );
        }
    }
    // Grace: a fresh exporter is measured from its start, not from "never".
    let baseline = inputs.last_success_at.or(inputs.started_at);
    if let Some(baseline) = baseline {
        if now.signed_duration_since(baseline) > inputs.window {
            let detail = if inputs.last_success_at.is_some() {
                format!("no successful OTLP export in the last {minutes}m")
            } else {
                format!("no successful OTLP export since start ({minutes}m window)")
            };
            return health(OtlpExportState::Failing, Some(detail));
        }
    }
    health(OtlpExportState::Ok, None)
}

/// Validate the exemption: `otlp_required == false` needs a non-empty reason.
/// Returns `(reason, warning)`; a missing reason yields no exemption plus a
/// warning for the caller to log.
#[must_use]
pub fn exemption(config: &ObservabilityConfig) -> (Option<String>, Option<String>) {
    if config.otlp_required != Some(false) {
        return (None, None);
    }
    match config.otlp_exempt_reason.as_deref().map(str::trim) {
        Some(reason) if !reason.is_empty() => (Some(reason.to_string()), None),
        _ => (
            None,
            Some(
                "observability.otlp_required=false ignored: observability.otlp_exempt_reason \
                 is empty (a deliberate exclusion must say why)"
                    .to_string(),
            ),
        ),
    }
}

/// The failure window from config (minutes, default 15, zero ignored).
#[must_use]
pub fn failure_window(config: &ObservabilityConfig) -> Duration {
    let minutes = config
        .otlp_failure_window_minutes
        .filter(|m| *m > 0)
        .unwrap_or(DEFAULT_FAILURE_WINDOW_MINUTES);
    Duration::minutes(i64::try_from(minutes).unwrap_or(i64::MAX / 120_000))
}

/// One observation's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub health: OtlpExportHealth,
    /// True on the first observation and whenever the state differs from the
    /// previous one: the caller logs the WARN exactly then.
    pub changed: bool,
}

/// Cross-sample state: drop-growth latch, grace anchor, last reported state.
#[derive(Debug, Default)]
pub struct OtlpHealthMonitor {
    first_seen_at: Option<DateTime<Utc>>,
    last_dropped: Option<u64>,
    last_drop_growth_at: Option<DateTime<Utc>>,
    last_state: Option<OtlpExportState>,
}

impl OtlpHealthMonitor {
    /// Fold one sample of the OTLP queue's `dropped_total` and the exporter's
    /// timestamps into `base` (whose `started_at` / `last_drop_growth_at` are
    /// filled here), evaluate, and report whether the state changed. Repeating
    /// the same `dropped_total` does not move the latch.
    pub fn observe(
        &mut self,
        mut base: OtlpHealthInputs,
        dropped_total: Option<u64>,
        now: DateTime<Utc>,
    ) -> Observation {
        let first_seen = *self.first_seen_at.get_or_insert(now);
        if let Some(dropped) = dropped_total {
            // A decrease is a counter reset or a reading applied out of order
            // by a concurrent caller, not growth. Keep the high-water mark so
            // a late stale reading cannot make the next one double-count.
            let prev = self.last_dropped;
            if prev.is_some_and(|p| dropped > p) {
                self.last_drop_growth_at = Some(now);
            }
            self.last_dropped = Some(prev.map_or(dropped, |p| p.max(dropped)));
        }
        base.started_at = base.started_at.or(Some(first_seen));
        base.last_drop_growth_at = self.last_drop_growth_at;
        let health = evaluate(&base, now);
        let changed = self.last_state != Some(health.state);
        self.last_state = Some(health.state);
        Observation { health, changed }
    }
}

/// The WARN line for a state that needs attention; `None` for `ok`/`exempt`.
#[must_use]
pub fn warn_line(health: &OtlpExportHealth) -> Option<String> {
    let detail = health.detail.as_deref().unwrap_or("");
    match health.state {
        OtlpExportState::NoExporter | OtlpExportState::Failing => Some(format!(
            "observability: otlp_export={} ({detail}); this daemon's facts are not reaching \
             SigNoz. Declare an `otlp` exporter or set observability.otlp_required=false with \
             observability.otlp_exempt_reason",
            health.state.as_str()
        )),
        _ => None,
    }
}

/// The INFO line logged when the state changes to one that needs no attention
/// (recovery to `ok`, entry into `exempt`); `None` for the WARN states.
#[must_use]
pub fn transition_line(health: &OtlpExportHealth) -> Option<String> {
    let detail = health.detail.as_deref().unwrap_or("");
    match health.state {
        OtlpExportState::Ok | OtlpExportState::Exempt => {
            Some(format!("observability: otlp_export={} ({detail})", health.state.as_str()))
        }
        _ => None,
    }
}

/// The line to log for `observation`: nothing for a repeated sample, otherwise
/// the WARN or INFO line for the new state. Startup counts as a change.
#[must_use]
pub fn log_line(observation: &Observation) -> Option<(log::Level, String)> {
    if !observation.changed {
        return None;
    }
    warn_line(&observation.health)
        .map(|l| (log::Level::Warn, l))
        .or_else(|| transition_line(&observation.health).map(|l| (log::Level::Info, l)))
}

static MONITOR: Mutex<Option<OtlpHealthMonitor>> = Mutex::new(None);

/// Evaluate this process's OTLP export health from `config` and the
/// process-global exporter status/queues, logging one WARN per state change.
pub fn check_global(config: &ObservabilityConfig) -> OtlpExportHealth {
    let now = Utc::now();
    let (exempt_reason, warning) = exemption(config);
    let statuses = super::global_export_statuses();
    let otlp = statuses.get(OTLP_EXPORTER);
    let dropped = super::global_export_queue_stats()
        .get(OTLP_EXPORTER)
        .map(|s| s.dropped_total);
    let inputs = OtlpHealthInputs {
        feature_otlp: cfg!(feature = "otlp"),
        otlp_planned: !super::planned_otlp_exporters(config).is_empty(),
        exempt_reason,
        started_at: otlp.and_then(|s| s.started_at),
        last_success_at: otlp.and_then(|s| s.last_success_at),
        last_drop_growth_at: None,
        window: failure_window(config),
    };
    let observation = MONITOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(OtlpHealthMonitor::default)
        .observe(inputs, dropped, now);
    if observation.changed {
        if let Some(warning) = warning {
            log::warn!("{warning}");
        }
        if let Some((level, line)) = log_line(&observation) {
            log::log!(level, "{line}");
        }
    }
    observation.health
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "otlp_health_tests.rs"]
mod tests;
