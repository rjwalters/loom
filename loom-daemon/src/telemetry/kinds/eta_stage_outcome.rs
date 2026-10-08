//! `eta.stage_outcome` (#10929): one stage an item actually left. Each record
//! carries the stage's entry and exit instants and how the item left it, and
//! links to the estimates that were open for the item at that moment.
//!
//! # Why this kind exists
//!
//! `eta.estimate` forecasts each stage still ahead (`stage_predictions`).
//! This kind records what each stage actually did, one record per stage
//! boundary as it happens. Before it, those boundaries stayed in the authority
//! host's local stage journal, and SigNoz saw only aggregate dwell
//! (`observability::ops::stage_dwell`). The per-estimate comparison of
//! predicted and actual time for each stage rides on `eta.outcome`'s
//! `attribution`. This record is the live per-item timeline it is built
//! from.
//!
//! # No new forge read
//!
//! A record is built only from the stage-journal rows the ETA tracker already
//! writes at each boundary ([`from_journal`]). The sources are bus phases,
//! listing diffs, verdicts, hold overlays and the `pr.resolved` read.
//!
//! # Joining to estimates
//!
//! `estimate_ids` names, for each `(kind, heuristic)` series, the newest
//! estimate still open for the item that was made strictly before `left_at`.
//! The list is bounded by the registry, not by how often the series
//! refreshed. `open_estimates` counts every open estimate. Any other
//! estimate of the item joins on `(repo, issue)` with `as_of < left_at`.
//!
//! **OTLP only**, and emitted only by the fleet's ETA authority (#10498),
//! because only the authority runs the tracker. **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::eta::journal::JournalEntry;
use crate::eta::score::EstimateSummary;
use crate::eta::{Kind, Provenance, Stage};

/// How the item left the stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageExit {
    /// On to the next stage of the path.
    Advance,
    /// A Judge approval.
    Pass,
    /// A Judge rejection: on to `doctor`.
    Rework,
    /// Approved, and an operator hold landed (`merge_wait` → `merge_hold`).
    Hold,
    /// The hold was released (`merge_hold` → `merge_wait`).
    Released,
    /// The sweep's Judge phase completed, and the verdict was not yet known.
    Judged,
    /// The work landed (a merge).
    Landed,
    /// Cut short: the PR closed unmerged, or the stage ended without
    /// completing. There is no dwell.
    CutShort,
    /// Nothing the row records says how.
    Unknown,
}

impl StageExit {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            StageExit::Advance => "advance",
            StageExit::Pass => "pass",
            StageExit::Rework => "rework",
            StageExit::Hold => "hold",
            StageExit::Released => "released",
            StageExit::Judged => "judged",
            StageExit::Landed => "landed",
            StageExit::CutShort => "cut_short",
            StageExit::Unknown => "unknown",
        }
    }

    /// The exit a stage-closing journal row records.
    #[must_use]
    pub fn of(row: &JournalEntry) -> Self {
        let state = row.raw["state"].as_str();
        let phase = row.raw["phase"].as_str();
        if state == Some("merged") || phase == Some("merge") {
            return StageExit::Landed;
        }
        if matches!(state, Some("closed" | "open")) || row.censored_sec.is_some() {
            return StageExit::CutShort;
        }
        match row.verdict.as_deref() {
            Some("pass") => return StageExit::Pass,
            Some("fail") => return StageExit::Rework,
            _ => {}
        }
        match (row.stage, row.next_stage) {
            (_, Some(Stage::Doctor)) => StageExit::Rework,
            (_, Some(Stage::MergeHold)) => StageExit::Hold,
            (Some(Stage::MergeHold), Some(Stage::MergeWait)) => StageExit::Released,
            (_, Some(_)) => StageExit::Advance,
            (Some(Stage::ReviewWait), None) if phase == Some("judge") => StageExit::Judged,
            _ => StageExit::Unknown,
        }
    }
}

/// One stage an item left.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtaStageOutcomeRecord {
    /// `owner/repo`.
    pub repo: String,
    /// GitHub numeric repo id, when an open estimate knew it (for the story
    /// trace context).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<u64>,
    /// The issue.
    pub issue: u32,
    /// The PR, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u32>,
    /// The stage left.
    pub stage: Stage,
    /// When it was entered, when that was observed exactly. Absent for a
    /// stage first seen mid-way (a restart, a first listing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entered_at: Option<DateTime<Utc>>,
    /// When it was left: the event time.
    pub left_at: DateTime<Utc>,
    /// `left_at − entered_at`, when the stage completed and its entry was
    /// exact. Never a lower bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dwell_sec: Option<i64>,
    /// How it was left.
    pub exit: StageExit,
    /// The stage entered next, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_stage: Option<Stage>,
    /// The journal event that recorded it (`sweep.phase`,
    /// `label.transition`, `pr.resolved`, …).
    pub event: String,
    /// When this daemon observed it: the knowable-at time.
    pub observed_at: DateTime<Utc>,
    /// How late `left_at` can be, in seconds (a listing interval, or `0` for a
    /// bus event).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_sec: Option<i64>,
    /// Every estimate of the item still open with `as_of < left_at`.
    pub open_estimates: usize,
    /// The newest such estimate per `(kind, heuristic)` series, in series
    /// order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub estimate_ids: Vec<String>,
    /// The observing build.
    pub loom: Provenance,
}

impl EtaStageOutcomeRecord {
    /// Whether the record carries valid provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.loom.is_valid()
    }
}

/// The `eta.stage_outcome` records for one batch of stage-journal rows,
/// observed at `observed_at`.
///
/// `open` is every estimate that was open for any item before the batch was
/// applied: the tracker's pending set plus the estimates the same batch
/// resolved (a merge scores and drops its `land` estimates before the
/// caller sees the rows). Only rows that close a stage of an issue give a
/// record. A slot-turnover sample has no issue and gives none.
#[must_use]
pub fn from_journal<'a>(
    rows: &[JournalEntry],
    open: impl Iterator<Item = &'a EstimateSummary> + Clone,
    observed_at: DateTime<Utc>,
    loom: &Provenance,
) -> Vec<EtaStageOutcomeRecord> {
    let mut records = Vec::new();
    for row in rows {
        let (Some(issue), Some(stage), Some(left_at)) = (row.issue, row.stage, row.left_at) else {
            continue;
        };
        let mut newest: BTreeMap<(Kind, &str), &EstimateSummary> = BTreeMap::new();
        let mut count = 0;
        let mut repo_id = None;
        for estimate in open.clone().filter(|e| {
            e.issue == issue && e.repo.eq_ignore_ascii_case(&row.repo) && e.as_of < left_at
        }) {
            count += 1;
            repo_id = repo_id.or(estimate.repo_id);
            let slot = newest
                .entry((estimate.kind, estimate.heuristic.as_str()))
                .or_insert(estimate);
            if (estimate.as_of, &estimate.estimate_id) > (slot.as_of, &slot.estimate_id) {
                *slot = estimate;
            }
        }
        records.push(EtaStageOutcomeRecord {
            repo: row.repo.clone(),
            repo_id,
            issue,
            pr_number: row.pr_number,
            stage,
            entered_at: row.entered_at,
            left_at,
            dwell_sec: row.duration_sec,
            exit: StageExit::of(row),
            next_stage: row.next_stage,
            event: row.event.clone(),
            observed_at,
            resolution_sec: row.resolution_sec,
            open_estimates: count,
            estimate_ids: newest.values().map(|e| e.estimate_id.clone()).collect(),
            loom: loom.clone(),
        });
    }
    records
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "eta_stage_outcome_tests.rs"]
mod tests;
