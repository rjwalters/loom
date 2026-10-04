//! `land` replay cases derived from a merged PR's forge label timeline
//! (#9579).
//!
//! # Why this source exists
//!
//! [`super::cases_from_record`] can derive a [`Kind::Land`] case only from a
//! `sweep.outcome` record whose phase sequence ends in `merge`. The fleet
//! merges out of sweep (Champion's auto-merge after the sweep has
//! terminated), so on a real host that set is empty and `land-v1` backtests
//! against nothing. A merged PR's own label timeline — the same
//! [`PrHistory`] `eta backfill` already reads — carries exactly what a `land`
//! case needs:
//!
//! - **`as_of`**: every stage *entry* the timeline records —
//!   `loom:review-requested` (`review_wait`), `loom:changes-requested`
//!   (`doctor`) and `loom:pr` (`merge_wait`) — via
//!   [`entry_transitions`], so a re-applied label is one entry while a real
//!   second lap is another;
//! - **`actual_at`**: [`PrHistory::merged_at`];
//! - **`outcome`**: [`OutcomeKind::Landed`];
//! - **`subject`**: the PR number from the history, and the issue from the
//!   forge's own closing reference, which the history does not carry and
//!   which is therefore an explicit input here ([`PrCaseRecord::closing_issues`]).
//!
//! # What is excluded, and why it is not guessed
//!
//! A PR that cannot honestly yield a case is reported as a
//! [`PrCaseExclusion`], never fabricated into one: an incomplete timeline
//! (a short log is not a fast PR), an open PR (no terminal yet), a PR closed
//! unmerged (never a landing — and not an `Abandoned` case either, because
//! [`OutcomeKind::Abandoned`] is decided by the *issue*, which may still land
//! through another PR), a merged PR with no merge instant, a terminal earlier
//! than the PR's own creation, a missing or ambiguous issue identity (the PR
//! number is never substituted for the issue), and a PR with no stage entry
//! before its merge.
//!
//! # Leak-freedom, re-established for this source
//!
//! [`super`]'s argument for sweep-derived cases (a record's own samples are
//! observed at its `emitted_at`, never earlier than its own cases) does not
//! carry over: a forge case's `as_of` is a label-event instant. The argument
//! here is per sample. A backfilled row ([`super::super::journal::entries_from_pr_history`])
//! is observed at the instant its segment *ended* (`left_at`): the PR's own
//! `merge_wait` row at `merged_at`, which is strictly after every case this
//! module emits for that PR (cases are only emitted for entries strictly
//! before `merged_at`); a lap's `review_wait` row at its verdict, which is
//! strictly after that lap's own entry. [`super::run`] reaches history only
//! through `select`/`select_at` (`observed_at < as_of`), so a PR's own future
//! segments are excluded by the same structural rule as everything else.
//! Each case's predictor inputs — `as_of`, `stage`, `rework_rounds` — are
//! computed only from events at or before its entry, so later labels,
//! rejections and the final rework count cannot reach them;
//! `tests/backtest_pr.rs` pins both properties.

use super::ReplayCase;
use crate::eta::journal::entry_transitions;
use crate::eta::score::OutcomeKind;
use crate::eta::{Kind, Stage, Subject};
use crate::pr_latency::history::{PrEvent, PrState};
use crate::pr_latency::{PrHistory, APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Schema tag of one [`PrCaseRecord`].
pub const PR_CASE_SCHEMA: &str = "eta-pr-case/v1";

/// One PR timeline event, in the serialisable shape the offline case input
/// (`eta backtest --pr-history`) uses. Mirrors [`PrEvent`] one-for-one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PrEventRecord {
    /// A label was applied.
    Labeled {
        /// The label.
        label: String,
        /// When.
        at: DateTime<Utc>,
    },
    /// A label was removed.
    Unlabeled {
        /// The label.
        label: String,
        /// When.
        at: DateTime<Utc>,
    },
    /// New commits reached the head branch.
    Pushed {
        /// When.
        at: DateTime<Utc>,
    },
    /// The PR merged.
    Merged {
        /// When.
        at: DateTime<Utc>,
    },
}

impl From<&PrEvent> for PrEventRecord {
    fn from(e: &PrEvent) -> Self {
        match e {
            PrEvent::Labeled { label, at } => Self::Labeled {
                label: label.clone(),
                at: *at,
            },
            PrEvent::Unlabeled { label, at } => Self::Unlabeled {
                label: label.clone(),
                at: *at,
            },
            PrEvent::Pushed { at } => Self::Pushed { at: *at },
            PrEvent::Merged { at } => Self::Merged { at: *at },
        }
    }
}

impl From<&PrEventRecord> for PrEvent {
    fn from(e: &PrEventRecord) -> Self {
        match e {
            PrEventRecord::Labeled { label, at } => Self::Labeled {
                label: label.clone(),
                at: *at,
            },
            PrEventRecord::Unlabeled { label, at } => Self::Unlabeled {
                label: label.clone(),
                at: *at,
            },
            PrEventRecord::Pushed { at } => Self::Pushed { at: *at },
            PrEventRecord::Merged { at } => Self::Merged { at: *at },
        }
    }
}

/// One PR's history plus the identity its [`PrHistory`] does not carry: the
/// offline (fixture / cache) form of the forge case source, and what the
/// forge acquisition writes with `--save-pr-history`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrCaseRecord {
    /// Always [`PR_CASE_SCHEMA`].
    #[serde(default = "default_schema")]
    pub schema: String,
    /// `owner/repo`.
    pub repo: String,
    /// The PR number.
    pub number: u32,
    /// When the PR was opened.
    pub created_at: DateTime<Utc>,
    /// `open` / `merged` / `closed` (the `gh` vocabulary is accepted too).
    pub state: String,
    /// When it merged, if it did.
    #[serde(default)]
    pub merged_at: Option<DateTime<Utc>>,
    /// The issues the forge says this PR closes, in this repo. `None` when
    /// the closing reference was never read — distinct from `Some([])`, a
    /// read that found none. Either is an excluded case, never a guess.
    #[serde(default)]
    pub closing_issues: Option<Vec<u32>>,
    /// `false` when the timeline read did not fully answer.
    pub timeline_complete: bool,
    /// The timeline, any order (sorted on conversion).
    #[serde(default)]
    pub events: Vec<PrEventRecord>,
}

fn default_schema() -> String {
    PR_CASE_SCHEMA.to_string()
}

impl PrCaseRecord {
    /// The record for an already-fetched history.
    #[must_use]
    pub fn from_history(repo: &str, h: &PrHistory, closing_issues: Option<Vec<u32>>) -> Self {
        PrCaseRecord {
            schema: PR_CASE_SCHEMA.to_string(),
            repo: repo.to_string(),
            number: h.number,
            created_at: h.created_at,
            state: h.state.as_str().to_string(),
            merged_at: h.merged_at,
            closing_issues,
            timeline_complete: h.timeline_complete,
            events: h.events.iter().map(PrEventRecord::from).collect(),
        }
    }

    /// The [`PrHistory`] this record describes.
    #[must_use]
    pub fn history(&self) -> PrHistory {
        PrHistory::new(
            self.number,
            self.created_at,
            PrState::parse(&self.state),
            self.merged_at,
            Vec::new(),
            self.events.iter().map(PrEvent::from).collect(),
            self.timeline_complete,
        )
    }
}

/// Parse an offline case input: either one JSON array of [`PrCaseRecord`]s
/// or JSON Lines, one record per line.
///
/// # Errors
///
/// The text is neither shape; the message names the first bad line.
pub fn parse_pr_records(text: &str) -> Result<Vec<PrCaseRecord>, String> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('[') {
        return serde_json::from_str(trimmed).map_err(|e| format!("unreadable JSON array: {e}"));
    }
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| serde_json::from_str(l).map_err(|e| format!("line {}: {e}", i + 1)))
        .collect()
}

/// Why one PR yielded no `land` case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrCaseExclusion {
    /// The timeline read did not fully answer.
    IncompleteTimeline,
    /// Still open: no terminal to score against.
    Open,
    /// Closed without merging: never a landing.
    ClosedUnmerged,
    /// Marked merged but carries no merge instant.
    MissingMergedAt,
    /// The merge instant is earlier than the PR's own creation.
    InvalidTerminalOrder,
    /// The closing reference was not read, or names no issue in this repo.
    MissingIdentity,
    /// The closing reference names more than one issue.
    AmbiguousIdentity,
    /// No stage entry strictly before the merge.
    NoStageEntry,
}

impl PrCaseExclusion {
    /// The stable `snake_case` name, as reported.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IncompleteTimeline => "incomplete_timeline",
            Self::Open => "open",
            Self::ClosedUnmerged => "closed_unmerged",
            Self::MissingMergedAt => "missing_merged_at",
            Self::InvalidTerminalOrder => "invalid_terminal_order",
            Self::MissingIdentity => "missing_identity",
            Self::AmbiguousIdentity => "ambiguous_identity",
            Self::NoStageEntry => "no_stage_entry",
        }
    }
}

/// Every `land` replay case one PR's history answers, or why it answers
/// none. Pure: `closing_issues` is resolved by the caller at the acquisition
/// boundary.
///
/// One case per stage entry strictly before the merge, each replayed from
/// its entry instant with the rejections already taken *then* (doctor
/// entries strictly earlier — the same counting [`super::cases_from_record`]
/// uses, so a `doctor` case carries the rounds before its own).
///
/// # Errors
///
/// The [`PrCaseExclusion`] that applies, checked in the order listed there.
pub fn cases_from_pr_history(
    repo: &str,
    h: &PrHistory,
    closing_issues: Option<&[u32]>,
) -> Result<Vec<ReplayCase>, PrCaseExclusion> {
    if !h.timeline_complete {
        return Err(PrCaseExclusion::IncompleteTimeline);
    }
    match h.state {
        PrState::Open => return Err(PrCaseExclusion::Open),
        PrState::Closed => return Err(PrCaseExclusion::ClosedUnmerged),
        PrState::Merged => {}
    }
    let merged_at = h.merged_at.ok_or(PrCaseExclusion::MissingMergedAt)?;
    if merged_at < h.created_at {
        return Err(PrCaseExclusion::InvalidTerminalOrder);
    }
    let issue = {
        let mut issues = closing_issues
            .ok_or(PrCaseExclusion::MissingIdentity)?
            .to_vec();
        issues.sort_unstable();
        issues.dedup();
        match issues.as_slice() {
            [] => return Err(PrCaseExclusion::MissingIdentity),
            [one] => *one,
            _ => return Err(PrCaseExclusion::AmbiguousIdentity),
        }
    };

    let verdict_clears = |e: &PrEvent| {
        matches!(e, PrEvent::Labeled { label, .. }
            if label == APPROVED || label == CHANGES_REQUESTED)
    };
    let push_clears = |e: &PrEvent| matches!(e, PrEvent::Pushed { .. });
    // An approval stops being in force when the PR goes back to review or
    // is rejected, so a later re-approval is a genuine second `merge_wait`.
    let approval_clears = |e: &PrEvent| {
        matches!(e, PrEvent::Labeled { label, .. }
            if label == REVIEW_REQUESTED || label == CHANGES_REQUESTED)
    };

    let mut entries: Vec<(DateTime<Utc>, Stage)> = Vec::new();
    for (label, stage, clears) in [
        (
            REVIEW_REQUESTED,
            Stage::ReviewWait,
            &verdict_clears as &dyn Fn(&PrEvent) -> bool,
        ),
        (CHANGES_REQUESTED, Stage::Doctor, &push_clears),
        (APPROVED, Stage::MergeWait, &approval_clears),
    ] {
        entries.extend(
            entry_transitions(h, label, clears)
                .into_iter()
                .filter(|at| *at < merged_at)
                .map(|at| (at, stage)),
        );
    }
    if entries.is_empty() {
        return Err(PrCaseExclusion::NoStageEntry);
    }
    entries.sort();

    let mut subject = Subject::new(repo, None, issue);
    subject.pr_number = Some(h.number);
    let doctor_entries: Vec<DateTime<Utc>> = entries
        .iter()
        .filter(|(_, s)| *s == Stage::Doctor)
        .map(|(at, _)| *at)
        .collect();
    Ok(entries
        .into_iter()
        .map(|(as_of, stage)| ReplayCase {
            subject: subject.clone(),
            as_of,
            stage,
            rework_rounds: doctor_entries.iter().filter(|d| **d < as_of).count() as u32,
            kind: Kind::Land,
            outcome: OutcomeKind::Landed,
            actual_at: merged_at,
            dispatch: None,
        })
        .collect())
}

/// How a batch of PR records turned into cases: what was read, what
/// contributed, and every exclusion by reason.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PrCaseSummary {
    /// PR records read.
    pub prs: usize,
    /// Of those, how many yielded at least one case.
    pub contributing: usize,
    /// Cases derived (before any cross-source deduplication).
    pub cases: usize,
    /// Excluded PRs, by [`PrCaseExclusion::as_str`].
    pub excluded: BTreeMap<String, usize>,
}

/// [`cases_from_pr_history`] over every record, with the tally.
#[must_use]
pub fn cases_from_pr_records(records: &[PrCaseRecord]) -> (Vec<ReplayCase>, PrCaseSummary) {
    let mut summary = PrCaseSummary {
        prs: records.len(),
        ..PrCaseSummary::default()
    };
    let mut cases = Vec::new();
    for record in records {
        match cases_from_pr_history(
            &record.repo,
            &record.history(),
            record.closing_issues.as_deref(),
        ) {
            Ok(found) => {
                summary.contributing += 1;
                summary.cases += found.len();
                cases.extend(found);
            }
            Err(reason) => {
                *summary
                    .excluded
                    .entry(reason.as_str().to_string())
                    .or_insert(0) += 1;
            }
        }
    }
    (cases, summary)
}
