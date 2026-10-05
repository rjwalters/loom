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
//! - **`as_of`**: every stage *entry* the timeline records, where the stage
//!   is what [`stage_from_pr_labels`] — the one label → stage definition the
//!   tracker serves from (#10218) — resolves for the labels in force at that
//!   instant (#10305). The label events are replayed with
//!   [`super::super::episodes`]' shared traversal: every event at one instant
//!   is applied in timeline order and the stage resolved once, so a
//!   `--remove-label --add-label` edit is one transition, a re-applied label
//!   or an unrelated label is no entry, and a real second lap is another.
//!   An approved PR that gets an operator hold enters `merge_hold`, and
//!   re-enters `merge_wait` at the release; an approval landing under a hold
//!   enters `merge_hold`, never `merge_wait`;
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
//! number is never substituted for the issue), a PR with no stage entry
//! before its merge, and a PR every one of whose entries was refused.
//!
//! An entry the resolver refuses — a review lap under a non-merge hold such
//! as `loom:blocked` (`blocked`), or contradictory review labels
//! (`unknown_stage`) — yields no case and no guessed stage: it is counted in
//! [`PrCaseSummary::refused_entries`] by reason, and the PR's other entries
//! still yield their cases. A rejection lap whose entry was refused still
//! counts toward later cases' `rework_rounds`: the rework happened.
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
//! computed only from events at or before its entry (the replay is causal:
//! each instant's resolution reads only the labels in force after it), so
//! later labels, rejections and the final rework count cannot reach them;
//! `tests/backtest_pr.rs` pins both properties.

use super::ReplayCase;
use crate::eta::episodes::{input_from_pr_history, replay};
use crate::eta::labels::{
    hold_labels, stage_from_pr_labels, APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED, TREATING,
};
use crate::eta::score::OutcomeKind;
use crate::eta::{Kind, NoEstimateReason, Stage, Subject};
use crate::pr_latency::history::{PrEvent, PrState};
use crate::pr_latency::PrHistory;
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
    /// Stage entries before the merge, but [`stage_from_pr_labels`] refused
    /// every one (each is counted in [`PrCaseSummary::refused_entries`]).
    NoUsableEntry,
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
            Self::NoUsableEntry => "no_usable_entry",
        }
    }
}

/// One stage entry the shared resolver refused: no case is emitted for it and
/// no stage is guessed. `reason` is [`stage_from_pr_labels`]'s own refusal —
/// `blocked` (a review lap under a non-merge hold) or `unknown_stage`
/// (contradictory review labels).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusedEntry {
    /// The instant the labels entered the refused resolution.
    pub at: DateTime<Utc>,
    /// Why it has no stage.
    pub reason: NoEstimateReason,
}

/// Every stage entry one PR's history answers strictly before its merge: the
/// usable ones as cases, the refused ones by reason.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrCaseEntries {
    /// One `land` case per resolved stage entry, in entry order.
    pub cases: Vec<ReplayCase>,
    /// Every refused entry, in entry order.
    pub refused: Vec<RefusedEntry>,
}

/// The review labels: a label set carrying none of them names no review
/// stage at all (a fresh or fully unlabeled PR), which is neither an entry
/// nor a refusal. Which stage they name is [`stage_from_pr_labels`]' call.
const REVIEW_LABELS: [&str; 4] = [REVIEW_REQUESTED, CHANGES_REQUESTED, APPROVED, TREATING];

/// The merge instant and closing issue of a PR that can be a `land` case.
fn land_terminal(
    h: &PrHistory,
    closing_issues: Option<&[u32]>,
) -> Result<(DateTime<Utc>, u32), PrCaseExclusion> {
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
    let mut issues = closing_issues
        .ok_or(PrCaseExclusion::MissingIdentity)?
        .to_vec();
    issues.sort_unstable();
    issues.dedup();
    match issues.as_slice() {
        [] => Err(PrCaseExclusion::MissingIdentity),
        [one] => Ok((merged_at, *one)),
        _ => Err(PrCaseExclusion::AmbiguousIdentity),
    }
}

/// Every stage entry one PR's history answers before its merge — cases and
/// refusals — or why it answers none. Pure: `closing_issues` is resolved by
/// the caller at the acquisition boundary.
///
/// The label events strictly before the merge are replayed with the episode
/// derivation's traversal ([`replay`]); at each instant the labels in force
/// are resolved with [`stage_from_pr_labels`]. An entry is an instant whose
/// resolution differs from the previous instant's: `Ok(stage)` is a case,
/// `Err(reason)` a [`RefusedEntry`] (also when a refused lap moves to another
/// review stage under the same refusal, e.g. `loom:blocked` held through a
/// rejection). Each case carries the rejections already taken *then* — the
/// instants the labels, holds ignored, newly named `doctor`, strictly before
/// its entry — so a rejection lap counts even when its own entry was refused.
///
/// # Errors
///
/// The [`PrCaseExclusion`] that applies, checked in the order listed there;
/// [`PrCaseExclusion::NoStageEntry`] when nothing before the merge names a
/// review stage at all.
pub fn pr_case_entries(
    repo: &str,
    h: &PrHistory,
    closing_issues: Option<&[u32]>,
) -> Result<PrCaseEntries, PrCaseExclusion> {
    let (merged_at, issue) = land_terminal(h, closing_issues)?;

    let holds = hold_labels();
    let mut entries: Vec<(DateTime<Utc>, Result<Stage, NoEstimateReason>)> = Vec::new();
    let mut rejections: Vec<DateTime<Utc>> = Vec::new();
    // `None`: no review label in force (nothing staged yet, or unlabeled).
    let mut prev_resolved: Option<Result<Stage, NoEstimateReason>> = None;
    let mut prev_named: Option<Stage> = None;
    // Cut at the merge: only events strictly before it move a stage.
    replay(&input_from_pr_history(h, repo), merged_at, |at, present| {
        let staged = present.iter().any(|l| REVIEW_LABELS.contains(&l.as_str()));
        let resolved = staged.then(|| stage_from_pr_labels(present));
        // The review stage the labels name with every hold set aside: what a
        // refused lap *was*, so a held rejection still counts as rework.
        let unheld: Vec<String> = present
            .iter()
            .filter(|l| !holds.contains(&l.as_str()))
            .cloned()
            .collect();
        let named = stage_from_pr_labels(&unheld).ok();
        if named == Some(Stage::Doctor) && prev_named != Some(Stage::Doctor) {
            rejections.push(at);
        }
        match resolved {
            Some(Ok(stage)) if prev_resolved != Some(Ok(stage)) => entries.push((at, Ok(stage))),
            Some(Err(reason)) if prev_resolved != Some(Err(reason)) || prev_named != named => {
                entries.push((at, Err(reason)));
            }
            _ => {}
        }
        prev_resolved = resolved;
        prev_named = named;
    });
    if entries.is_empty() {
        return Err(PrCaseExclusion::NoStageEntry);
    }

    let mut subject = Subject::new(repo, None, issue);
    subject.pr_number = Some(h.number);
    let mut out = PrCaseEntries::default();
    for (as_of, resolved) in entries {
        match resolved {
            Ok(stage) => out.cases.push(ReplayCase {
                subject: subject.clone(),
                as_of,
                stage,
                rework_rounds: rejections.iter().filter(|r| **r < as_of).count() as u32,
                kind: Kind::Land,
                outcome: OutcomeKind::Landed,
                actual_at: merged_at,
                dispatch: None,
            }),
            Err(reason) => out.refused.push(RefusedEntry { at: as_of, reason }),
        }
    }
    Ok(out)
}

/// Every `land` replay case one PR's history answers, or why it answers
/// none: [`pr_case_entries`]' cases, one per resolved stage entry strictly
/// before the merge.
///
/// # Errors
///
/// [`pr_case_entries`]' exclusion, or [`PrCaseExclusion::NoUsableEntry`]
/// when every entry was refused.
pub fn cases_from_pr_history(
    repo: &str,
    h: &PrHistory,
    closing_issues: Option<&[u32]>,
) -> Result<Vec<ReplayCase>, PrCaseExclusion> {
    let entries = pr_case_entries(repo, h, closing_issues)?;
    if entries.cases.is_empty() {
        return Err(PrCaseExclusion::NoUsableEntry);
    }
    Ok(entries.cases)
}

/// How a batch of PR records turned into cases: what was read, what
/// contributed, every PR exclusion by reason, and every refused entry by
/// reason.
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
    /// Refused stage *entries* (not PRs), by [`NoEstimateReason::as_str`]:
    /// counted for contributing PRs too, whose other entries still yield
    /// their cases.
    #[serde(default)]
    pub refused_entries: BTreeMap<String, usize>,
}

/// [`pr_case_entries`] over every record, with the tally.
#[must_use]
pub fn cases_from_pr_records(records: &[PrCaseRecord]) -> (Vec<ReplayCase>, PrCaseSummary) {
    let mut summary = PrCaseSummary {
        prs: records.len(),
        ..PrCaseSummary::default()
    };
    let mut cases = Vec::new();
    for record in records {
        let reason = match pr_case_entries(
            &record.repo,
            &record.history(),
            record.closing_issues.as_deref(),
        ) {
            Ok(found) => {
                for refused in &found.refused {
                    *summary
                        .refused_entries
                        .entry(refused.reason.as_str().to_string())
                        .or_insert(0) += 1;
                }
                if found.cases.is_empty() {
                    PrCaseExclusion::NoUsableEntry
                } else {
                    summary.contributing += 1;
                    summary.cases += found.cases.len();
                    cases.extend(found.cases);
                    continue;
                }
            }
            Err(reason) => reason,
        };
        *summary
            .excluded
            .entry(reason.as_str().to_string())
            .or_insert(0) += 1;
    }
    (cases, summary)
}
