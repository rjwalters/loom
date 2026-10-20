//! This host's alerts, as `alert.transition` OTLP log records (#11295,
//! #12806).
//!
//! Every alerting source in the daemon (fleet_alert, the self-update rolls,
//! fleet sync, the credential preflight, the install self-check, task
//! liveness, the work-finder guard, the safehoused, breaker and llm-monitor
//! watches) raises an [`Alert`] through [`post`]. Each becomes one
//! [`AlertTransitionRecord`] offered to the daemon's own OTLP exporters, the
//! same path as every other fact. loom-ui reads them from SigNoz (its
//! `docs/alerts/external.md` § "As telemetry (shadow)" is the schema). The
//! daemon never calls the dashboard: until #12806 this module POSTed to
//! loom-ui's `/api/alerts/external` with the host ingest key, and that path is
//! gone.
//!
//! # Contract
//!
//! `source` is this host's id ([`source_host`]), the alert kind is `rule`, the
//! entity `key`. `open` opens, updates or reopens (an idempotent upsert keyed
//! by `(source, rule, key)`); `resolved` closes. `observed_at` is stamped when
//! [`post`] is called, so an alert raised before the exporter is up still
//! carries the time it was observed.
//!
//! # No OTLP exporter means off
//!
//! [`start`] decides once, after the exporters have started: with no OTLP
//! exporter registered every [`post`] is a no-op. An alert posted before
//! [`start`] waits in a bounded buffer ([`PENDING_CAPACITY`]), so the startup
//! checks that run first (credential preflight, the fleet-sync startup pass)
//! are not lost.
//!
//! # Heartbeat
//!
//! loom-ui closes an incident it has not seen observed for 24 h, so while an
//! alert holds its `open` is re-sent every [`HEARTBEAT`] (well inside the
//! schema's 12 h). The heartbeat covers every `open` this process posted and
//! the incidents a [`ledger::Ledger`] reloaded from an earlier process.
//!
//! # Open incidents across a restart
//!
//! A level-triggered source keeps a [`ledger::Ledger`]: the incidents it has
//! open, persisted, so a daemon restarted while one was open resolves it on its
//! first healthy evaluation. A single-key watch can use [`sustain::Tracker`],
//! which owes one `resolved` at its first healthy sample instead.
//!
//! The watchdog, a separate one-shot process that runs while the daemon is
//! down, has its own durable path (`watchdog::loom_ui`).

pub mod ledger;
pub mod sanitize;
pub mod sustain;
pub mod watches;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

pub use crate::telemetry::kinds::alert_transition::AlertTransitionRecord;
use crate::telemetry::TelemetryRecord;

/// How often an `open` that still holds is re-sent. The schema asks for at
/// least every 12 h; half that leaves room for a restart or a slow export.
pub const HEARTBEAT: Duration = Duration::from_secs(6 * 60 * 60);
/// How often the heartbeat task looks for an `open` that is due.
pub const HEARTBEAT_CHECK: Duration = Duration::from_secs(15 * 60);
/// Alerts held for [`start`]; past this the oldest is dropped.
pub const PENDING_CAPACITY: usize = 256;

/// `warning` or `critical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Critical,
}

impl Severity {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// `open` or `resolved`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    Resolved,
}

impl State {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
        }
    }
}

/// One alert transition. Fields are raw; [`record`] sanitizes them to the
/// schema's charsets and lengths, so callers never have to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub rule: String,
    pub key: String,
    pub title: String,
    pub severity: Severity,
    pub state: State,
    pub detail: Option<String>,
    pub link: Option<String>,
}

impl Alert {
    #[must_use]
    pub fn new(
        rule: impl Into<String>,
        key: impl Into<String>,
        title: impl Into<String>,
        severity: Severity,
        state: State,
    ) -> Self {
        Self {
            rule: rule.into(),
            key: key.into(),
            title: title.into(),
            severity,
            state,
            detail: None,
            link: None,
        }
    }

    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    #[must_use]
    pub fn with_link(mut self, link: impl Into<String>) -> Self {
        self.link = Some(link.into());
        self
    }
}

/// The `alert.transition` record for `alert` from `source`, observed at
/// `observed_at`, every field sanitized. An empty detail and an unusable link
/// are omitted (the reader keeps the earlier one).
#[must_use]
pub fn record(alert: &Alert, source: &str, observed_at: DateTime<Utc>) -> AlertTransitionRecord {
    AlertTransitionRecord {
        source: sanitize::ident(source),
        rule: sanitize::ident(&alert.rule),
        key: sanitize::key(&alert.key),
        state: alert.state.as_str().to_string(),
        severity: alert.severity.as_str().to_string(),
        title: sanitize::title(&alert.title, &alert.rule),
        observed_at,
        detail: alert
            .detail
            .as_deref()
            .map(sanitize::detail)
            .filter(|d| !d.is_empty()),
        link: alert.link.as_deref().and_then(sanitize::link),
    }
}

/// This host's id: the one the OTLP Resource carries as `host.id`.
#[must_use]
pub fn source_host() -> String {
    crate::sweep_registry::host_identity()
}

/// Offer `alert` to the OTLP exporters, observed at `observed_at`.
fn emit(alert: &Alert, observed_at: DateTime<Utc>) {
    crate::observability::ops::emit_record(TelemetryRecord::AlertTransition(record(
        alert,
        &source_host(),
        observed_at,
    )));
}

/// [`start`] has not decided yet.
const UNDECIDED: u8 = 0;
/// [`start`] found an OTLP exporter: posts are emitted.
const ACTIVE: u8 = 1;
/// [`start`] found none: posts are dropped.
const OFF: u8 = 2;

/// One incident this process believes open, for the heartbeat.
#[derive(Debug, Clone)]
struct Held {
    alert: Alert,
    /// When this process last sent it; `None` for an adopted incident, which
    /// is due at once (the previous process's last send is not counted).
    last_sent: Option<Instant>,
}

/// The buffer before [`start`] and the open set the heartbeat re-sends.
#[derive(Debug, Default)]
struct Output {
    pending: VecDeque<(Alert, DateTime<Utc>)>,
    open: BTreeMap<(String, String), Held>,
}

impl Output {
    /// Keep the open set in step with `alert`.
    fn track(&mut self, alert: &Alert, now: Instant) {
        let id = (alert.rule.clone(), alert.key.clone());
        match alert.state {
            State::Open => {
                self.open.insert(
                    id,
                    Held {
                        alert: alert.clone(),
                        last_sent: Some(now),
                    },
                );
            }
            State::Resolved => {
                self.open.remove(&id);
            }
        }
    }

    /// Hold `alert` for [`start`]; returns the alert dropped to make room.
    fn hold(&mut self, alert: Alert, observed_at: DateTime<Utc>) -> Option<Alert> {
        let dropped = (self.pending.len() >= PENDING_CAPACITY)
            .then(|| self.pending.pop_front().map(|(a, _)| a))
            .flatten();
        self.pending.push_back((alert, observed_at));
        dropped
    }

    /// The opens whose last send is [`HEARTBEAT`] old at `now`, marked sent.
    fn due(&mut self, now: Instant) -> Vec<Alert> {
        self.open
            .values_mut()
            .filter(|h| {
                h.last_sent
                    .is_none_or(|sent| now.saturating_duration_since(sent) >= HEARTBEAT)
            })
            .map(|h| {
                h.last_sent = Some(now);
                h.alert.clone()
            })
            .collect()
    }

    /// Adopt an incident an earlier process opened, unless already held. It is
    /// due at once: the downtime since that process's last send could already
    /// exceed the schema's 12 h heartbeat rule.
    fn adopt(&mut self, alert: Alert) {
        self.open
            .entry((alert.rule.clone(), alert.key.clone()))
            .or_insert(Held {
                alert,
                last_sent: None,
            });
    }
}

static STATE: AtomicU8 = AtomicU8::new(UNDECIDED);
static OUTPUT: OnceLock<Mutex<Output>> = OnceLock::new();

fn output() -> std::sync::MutexGuard<'static, Output> {
    OUTPUT
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Decide the output once the exporters are up: active when an OTLP exporter
/// is registered (its handle is the heartbeat task), else off, with one log
/// line, the held alerts discarded and the persisted ledgers forgotten.
#[must_use]
pub fn start() -> Option<tokio::task::JoinHandle<()>> {
    let mut out = output();
    if STATE.load(Ordering::Acquire) != UNDECIDED {
        return None;
    }
    if !crate::observability::ops::records_exported() {
        log::info!("alerts: off — no OTLP exporter is running, so no alert.transition goes out");
        STATE.store(OFF, Ordering::Release);
        out.pending.clear();
        out.open.clear();
        drop(out);
        ledger::forget_all();
        return None;
    }
    STATE.store(ACTIVE, Ordering::Release);
    let held = out.pending.len();
    for (alert, observed_at) in std::mem::take(&mut out.pending) {
        emit(&alert, observed_at);
    }
    drop(out);
    log::info!("alerts: emitting alert.transition over OTLP (#12806); {held} held before start");
    Some(tokio::spawn(async {
        loop {
            // First, so an incident adopted before start goes out at once.
            heartbeat();
            tokio::time::sleep(HEARTBEAT_CHECK).await;
        }
    }))
}

/// Re-send every `open` whose heartbeat is due.
fn heartbeat() {
    let due = output().due(Instant::now());
    for alert in due {
        log::debug!("alerts: heartbeat {} {}", alert.rule, alert.key);
        emit(&alert, Utc::now());
    }
}

/// Whether [`start`] found an OTLP exporter.
#[must_use]
pub fn is_active() -> bool {
    STATE.load(Ordering::Acquire) == ACTIVE
}

/// Whether [`start`] has found the output off. Before [`start`] runs this is
/// `false`: an alert may still go out.
#[must_use]
pub fn is_off() -> bool {
    STATE.load(Ordering::Acquire) == OFF
}

/// Record `alert`, observed now. Never blocks on the network. Before
/// [`start`] it is held; once the output is off it is a no-op.
pub fn post(alert: Alert) {
    if is_off() {
        return;
    }
    let observed_at = Utc::now();
    let mut out = output();
    out.track(&alert, Instant::now());
    match STATE.load(Ordering::Acquire) {
        ACTIVE => emit(&alert, observed_at),
        UNDECIDED => {
            if let Some(lost) = out.hold(alert, observed_at) {
                log::warn!(
                    "alerts: {PENDING_CAPACITY} alerts held before start, dropped the oldest {} {} \
                     ({})",
                    lost.rule,
                    lost.key,
                    lost.state.as_str()
                );
            }
        }
        _ => {}
    }
}

/// A [`ledger::Ledger`] reloaded `alert` as open from an earlier process: keep
/// its heartbeat going until the ledger resolves it. Re-sent at once.
pub fn adopt(alert: Alert) {
    if !is_off() {
        output().adopt(alert);
        if is_active() {
            heartbeat();
        }
    }
}
