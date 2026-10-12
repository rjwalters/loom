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
//!
//! Slice D of #11527 (Issue #11557): the monitor also remembers **since when**
//! the host has needed attention (`since`, in memory — after a restart it is
//! "since start", which is honest), the `failing` detail carries the
//! exporter's own last failure (e.g. `OTLP HTTP 403 (credential refused …)`),
//! and a degraded start is named through one predicate, [`startup_verdict`].

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
    /// No OTLP exporter will run: not configured or observability off. (The
    /// exporter is compiled into every build since #10700.)
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
    /// When the state first became `failing` / `no_exporter` (Issue #11557);
    /// cleared on `ok` / `exempt`. Moving between the two attention states, or
    /// a new failure detail, does not reset it. Filled by
    /// [`OtlpHealthMonitor`]; [`evaluate`] alone leaves it `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
}

impl OtlpExportHealth {
    /// True for the states that mean "facts are not reaching SigNoz".
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        matches!(self.state, OtlpExportState::NoExporter | OtlpExportState::Failing)
    }

    /// `otlp: failing since <T> (<reason>)` for an attention state; `None` for
    /// `ok` / `exempt`. One wording for the log lines and `status`.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        if !self.needs_attention() {
            return None;
        }
        let since = self
            .since
            .map(|t| format!(" since {}", t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)))
            .unwrap_or_default();
        let reason = self.detail.as_deref().unwrap_or("no detail");
        Some(format!("otlp: {}{since} ({reason})", self.state.as_str()))
    }
}

/// Everything [`evaluate`] reads. Plain data so tests build it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtlpHealthInputs {
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
    /// The OTLP exporter's own most recent failure text (its status cell's
    /// `last_failure_detail`), appended to a `failing` detail. When the cell
    /// is `misconfigured` this is why the exporter never started.
    pub last_failure_detail: Option<String>,
}

/// The pure rule. `now` is injected.
#[must_use]
pub fn evaluate(inputs: &OtlpHealthInputs, now: DateTime<Utc>) -> OtlpExportHealth {
    let health = |state, detail: Option<String>| OtlpExportHealth {
        state,
        detail,
        since: None,
    };
    if let Some(reason) = &inputs.exempt_reason {
        return health(OtlpExportState::Exempt, Some(reason.clone()));
    }
    if !inputs.otlp_planned {
        let detail = inputs.last_failure_detail.as_deref().map_or_else(
            || "no usable `otlp` entry in observability.exporters".to_string(),
            |why| format!("otlp exporter not started: {why}"),
        );
        return health(OtlpExportState::NoExporter, Some(detail));
    }
    // The exporter's own last failure, when it has one, so `failing` names
    // the cause (a refused credential, a 5xx) and not only the symptom.
    let with_cause = |detail: String| match inputs.last_failure_detail.as_deref() {
        Some(cause) => format!("{detail}; last failure: {cause}"),
        None => detail,
    };
    let minutes = inputs.window.num_minutes();
    if let Some(grew) = inputs.last_drop_growth_at {
        if now.signed_duration_since(grew) <= inputs.window {
            return health(
                OtlpExportState::Failing,
                Some(with_cause(format!(
                    "export queue dropped records within the last {minutes}m"
                ))),
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
            return health(OtlpExportState::Failing, Some(with_cause(detail)));
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
    /// Start of the current attention interval (Issue #11557).
    since: Option<DateTime<Utc>>,
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
        let mut health = evaluate(&base, now);
        // One interval per stretch of attention: the first failing /
        // no_exporter sample opens it, later samples (and detail changes, and
        // failing <-> no_exporter moves) keep it, `ok` / `exempt` closes it.
        self.since = if health.needs_attention() {
            Some(self.since.unwrap_or(now))
        } else {
            None
        };
        health.since = self.since;
        let changed = self.last_state != Some(health.state);
        self.last_state = Some(health.state);
        Observation { health, changed }
    }
}

/// The WARN line for a state that needs attention; `None` for `ok`/`exempt`.
#[must_use]
pub fn warn_line(health: &OtlpExportHealth) -> Option<String> {
    health.summary().map(|summary| {
        format!(
            "observability: {summary}; this daemon's facts are not reaching SigNoz. Declare \
             an `otlp` exporter or set observability.otlp_required=false with \
             observability.otlp_exempt_reason"
        )
    })
}

/// The startup decision (Issue #11557): one predicate, so #11526's ruling on
/// refusing to start changes the `Degraded` branch at its single caller and
/// nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupVerdict {
    /// Exporting, in startup grace, or deliberately exempt.
    Ok,
    /// Every `otlp` entry is absent or misconfigured and the host is not
    /// exempt; the payload is the detail to name.
    Degraded(String),
}

/// Classify the health observed at daemon start. Only `no_exporter` is a
/// degraded start: a fresh exporter is in grace and cannot yet be `failing`.
#[must_use]
pub fn startup_verdict(health: &OtlpExportHealth) -> StartupVerdict {
    match health.state {
        OtlpExportState::NoExporter => StartupVerdict::Degraded(
            health
                .summary()
                .unwrap_or_else(|| health.state.as_str().to_string()),
        ),
        _ => StartupVerdict::Ok,
    }
}

/// The ERROR line naming a degraded start.
#[must_use]
pub fn degraded_line(detail: &str) -> String {
    format!("observability: DEGRADED — this daemon's facts are not reaching SigNoz: {detail}")
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

/// Fold one sample of the process-global exporter state into [`MONITOR`].
/// Returns the observation plus the exemption warning (if any) for the caller
/// to log.
fn observe_global(config: &ObservabilityConfig) -> (Observation, Option<String>) {
    let now = Utc::now();
    let (exempt_reason, warning) = exemption(config);
    let statuses = super::global_export_statuses();
    let otlp = statuses.get(OTLP_EXPORTER);
    // A `misconfigured` OTLP cell never runs, so it is `no_exporter`, not a
    // planned exporter waiting out its grace (Issue #11557).
    let misconfigured =
        otlp.is_some_and(|s| s.state == crate::types::ObservabilityExportState::Misconfigured);
    let dropped = super::global_export_queue_stats()
        .get(OTLP_EXPORTER)
        .map(|s| s.dropped_total);
    let inputs = OtlpHealthInputs {
        otlp_planned: !misconfigured && !super::planned_otlp_exporters(config).is_empty(),
        exempt_reason,
        started_at: otlp.and_then(|s| s.started_at),
        last_success_at: otlp.and_then(|s| s.last_success_at),
        last_drop_growth_at: None,
        window: failure_window(config),
        last_failure_detail: otlp.and_then(|s| s.last_failure_detail.clone()),
    };
    let observation = MONITOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(OtlpHealthMonitor::default)
        .observe(inputs, dropped, now);
    (observation, warning)
}

/// Evaluate this process's OTLP export health from `config` and the
/// process-global exporter status/queues, logging one WARN per state change.
pub fn check_global(config: &ObservabilityConfig) -> OtlpExportHealth {
    let (observation, warning) = observe_global(config);
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

/// The daemon-start check (Issue #11557): like [`check_global`], but a
/// degraded start is logged as the one ERROR line [`degraded_line`] instead of
/// the WARN, and the [`StartupVerdict`] is returned for the caller to act on.
/// `since` is set to this first observation — daemon start.
pub fn check_startup(config: &ObservabilityConfig) -> StartupVerdict {
    let (observation, warning) = observe_global(config);
    if let Some(warning) = warning {
        log::warn!("{warning}");
    }
    let verdict = startup_verdict(&observation.health);
    match &verdict {
        StartupVerdict::Degraded(detail) => log::error!("{}", degraded_line(detail)),
        StartupVerdict::Ok => {
            if let Some((level, line)) = log_line(&observation) {
                log::log!(level, "{line}");
            }
        }
    }
    verdict
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "otlp_health_tests.rs"]
mod tests;
