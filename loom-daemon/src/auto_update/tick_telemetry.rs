//! What one self-update tick decided ([`TickSummary`]), and the
//! `auto_update.tick` record built from it (Issue #10414).
//!
//! `run_tick` fills a summary as it goes and returns it. [`emit`] turns it into
//! one record and offers it to the OTLP-only ops sink. Without an OTLP exporter
//! nothing is registered and the call does nothing.

use std::time::Duration;

use chrono::{DateTime, Utc};

use super::supersede::ArmedRoll;
use super::{ArtifactResolution, UpdateCheck};
use crate::eta::Provenance;
use crate::telemetry::kinds::auto_update_tick::{
    AutoUpdateTickRecord, DrainSnapshot, TickDecisionKind,
};
use crate::telemetry::TelemetryRecord;

/// One tick's decision and the readings it was made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickSummary {
    /// What the tick decided.
    pub decision: TickDecisionKind,
    /// The note published to status (`last tick:`).
    pub note: String,
    /// A fetch's or rebuild's outcome (`success` / `retryable` / `terminal`).
    pub outcome: Option<&'static str>,
    /// The roll's drain-and-restart was accepted.
    pub roll_armed: bool,
    /// The artifact resolution the decision used.
    pub artifact: Option<ArtifactResolution>,
    /// The source-staleness reading, when the tick took one.
    pub check: Option<UpdateCheck>,
    /// The in-flight sweep count, when the tick read it.
    pub in_flight: Option<usize>,
    /// The drain armed at tick start.
    pub drain: DrainSnapshot,
}

impl TickSummary {
    /// A summary for a tick that started with `armed` as the armed roll.
    #[must_use]
    pub fn new(armed: Option<&ArmedRoll>) -> Self {
        Self {
            decision: TickDecisionKind::Skip,
            note: String::new(),
            outcome: None,
            roll_armed: false,
            artifact: None,
            check: None,
            in_flight: None,
            drain: armed.map_or_else(DrainSnapshot::default, |roll| DrainSnapshot {
                armed: true,
                pending: roll.pending,
                refusals: roll.refusals,
                target: roll.target.clone(),
            }),
        }
    }

    /// Close the summary with the tick's decision.
    #[must_use]
    pub fn finish(
        mut self,
        decision: TickDecisionKind,
        note: String,
        artifact: &ArtifactResolution,
        outcome: Option<&'static str>,
    ) -> Self {
        self.decision = decision;
        self.note = note;
        self.artifact = Some(artifact.clone());
        self.outcome = outcome;
        self
    }
}

/// The `auto_update.tick` record for `summary`.
#[must_use]
pub fn record(
    summary: &TickSummary,
    consecutive_failures: u32,
    host_id: &str,
    started_at: DateTime<Utc>,
    duration: Duration,
    loom: Provenance,
) -> AutoUpdateTickRecord {
    let resolved = match &summary.artifact {
        Some(ArtifactResolution::Resolved(info)) => Some(info),
        _ => None,
    };
    let at = crate::telemetry::trace::instant(started_at);
    AutoUpdateTickRecord {
        tick_id: crate::telemetry::trace::derived_hex(&["loom.auto_update.tick", host_id, &at], 32),
        started_at,
        decision: summary.decision,
        reason: summary.note.clone(),
        outcome: summary.outcome.map(str::to_string),
        roll_armed: summary.roll_armed,
        installed_version: resolved
            .and_then(|info| info.installed_version.clone())
            .or_else(|| Some(loom.version.clone())),
        target_version: resolved.map(|info| info.version.clone()),
        target_published_at: resolved.and_then(|info| info.published_at.clone()),
        commits_behind: summary.check.as_ref().and_then(|c| c.commits_behind),
        hours_behind: summary.check.as_ref().and_then(|c| c.hours_behind),
        in_flight: summary.in_flight.map(|n| n as u64),
        drain: summary.drain.clone(),
        consecutive_failures,
        duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        loom,
    }
}

/// Build and emit one tick's record through the OTLP-only ops sink. A record
/// whose provenance does not validate is dropped with a warning, never sent.
pub fn emit(
    summary: &TickSummary,
    consecutive_failures: u32,
    started_at: DateTime<Utc>,
    duration: Duration,
) {
    let host_id = crate::observability::ops::global_ops_sink()
        .map_or_else(crate::sweep_registry::host_identity, |sink| sink.host_id().to_string());
    let record = record(
        summary,
        consecutive_failures,
        &host_id,
        started_at,
        duration,
        Provenance::current(),
    );
    if !record.has_provenance() {
        log::warn!("auto_update: dropped auto_update.tick record: invalid provenance");
        return;
    }
    crate::observability::ops::emit_record(TelemetryRecord::AutoUpdateTick(record));
}
