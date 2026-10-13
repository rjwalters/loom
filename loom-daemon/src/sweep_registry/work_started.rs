//! A sweep's `stage.work_started` facts (Issue #11641): its Curator at
//! dispatch, and each later role when the sweep's checkpoint marker hands the
//! item to it. No forge read: the repo slug comes from the cache dispatch
//! already filled, the attempt and origin from the sweep's start facts.

use std::path::Path;

use chrono::{DateTime, Utc};

use super::*;
use crate::telemetry::kinds::work_started::{source, sweep_role_for_marker, WorkStartedRecord};
use crate::telemetry::provenance::Provenance;
use crate::telemetry::TelemetryRecord;

/// The checkpoint marker's own `timestamp` (written by `sweep-checkpoint
/// write`), the instant `fleet.state` dates the stage entry with.
pub(super) fn read_checkpoint_timestamp(path: &Path) -> Option<DateTime<Utc>> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    DateTime::parse_from_rfc3339(v.get("timestamp")?.as_str()?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// 1-based count of starts of `marker`'s stage among `markers` (every marker
/// this sweep was observed in, `marker` included): a Doctor loop enters
/// `review_wait` once per `builder-done` / `doctor-done`.
pub(super) fn stage_attempt<'a>(marker: &str, markers: impl IntoIterator<Item = &'a str>) -> u32 {
    let Some((stage, _)) = sweep_role_for_marker(Some(marker)) else {
        return 1;
    };
    let n = markers
        .into_iter()
        .filter(|m| sweep_role_for_marker(Some(m)).is_some_and(|(s, _)| s == stage))
        .count();
    u32::try_from(n.max(1)).unwrap_or(u32::MAX)
}

impl SweepRegistry {
    /// Emit `stage.work_started` for the role `marker` hands `issue` to (`None`:
    /// the Curator, at dispatch). `marker_at` is the marker's own timestamp.
    /// A no-op without an OTLP exporter, for `merge-done` and for an unknown
    /// marker.
    pub(super) fn emit_work_started(
        &self,
        sweep_id: &str,
        issue: u32,
        marker: Option<&str>,
        marker_at: Option<DateTime<Utc>>,
        pr_number: Option<u32>,
        stage_attempt: u32,
    ) {
        if !crate::observability::ops::records_exported() {
            return;
        }
        let Some((stage, role)) = sweep_role_for_marker(marker) else {
            return;
        };
        let Some(repo) = self.journal_repo_slug() else {
            return;
        };
        let now = Utc::now();
        let (started_at, how) = match (marker, marker_at) {
            (None, _) => {
                (self.entries.get(sweep_id).map_or(now, |i| i.started_at), source::SWEEP_DISPATCH)
            }
            (Some(_), Some(at)) => (at, source::SWEEP_CHECKPOINT),
            (Some(_), None) => (now, source::SWEEP_OBSERVED),
        };
        let facts = self.start_facts.get(sweep_id);
        crate::observability::ops::emit_record(TelemetryRecord::WorkStarted(WorkStartedRecord {
            repo: repo.to_ascii_lowercase(),
            issue: Some(issue),
            pr_number,
            stage,
            role: role.to_string(),
            sweep_id: sweep_id.to_string(),
            attempt_index: facts.and_then(|f| f.attempt_index),
            stage_attempt,
            dispatch_origin: facts.and_then(|f| f.dispatch_origin.clone()),
            started_at,
            source: how.to_string(),
            observed_at: now,
            loom: Provenance::current(),
        }));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "work_started_tests.rs"]
mod tests;
