//! The stage-sample journal, `.loom/logs/eta-stage-samples.jsonl`: every
//! stage boundary the ETA tracker observes, appended the moment it is seen,
//! with its raw fields.
//!
//! Eager and generous by design (operator decision on #9289): a transition is
//! written when it is observed, never batched to a later pass, and an event
//! that may or may not be a stage boundary is written anyway with `stage`
//! absent. Backtests read this file; estimators read only the rows that are
//! complete stage durations ([`JournalEntry::history_sample`]).
//!
//! Rows the sweep-outcome journal already carries (in-sweep phases) are
//! marked `in_sweep` and are not read back as history, so no duration is
//! counted twice. Rotation matches the outcome journal: 5 MiB or 30 days,
//! one `.1` generation.

use super::history::{SampleSource, StageSample, StageSamples, VerdictSample};
use super::{Provenance, Stage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// Schema tag of one journal row.
pub const JOURNAL_SCHEMA: &str = "eta-stage-sample/v1";

/// File name under `<workspace>/.loom/logs/`.
pub const JOURNAL_FILENAME: &str = "eta-stage-samples.jsonl";

/// Test seam: overrides the journal path.
pub const JOURNAL_PATH_ENV: &str = "LOOM_ETA_STAGE_JOURNAL_PATH";

/// One observed boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Always [`JOURNAL_SCHEMA`].
    pub schema: String,
    /// When the tracker observed it.
    pub observed_at: DateTime<Utc>,
    /// What was observed: `sweep.dispatch`, `sweep.phase`, `sweep.terminal`,
    /// `label.transition`, `label.first_seen`, `pr.resolved`, `verdict`.
    pub event: String,
    /// `owner/repo`.
    pub repo: String,
    /// The issue, when known.
    pub issue: Option<u32>,
    /// The PR, when known.
    pub pr_number: Option<u32>,
    /// The sweep, when one was involved.
    pub sweep_id: Option<String>,
    /// The stage this row completes, when it completes one.
    pub stage: Option<Stage>,
    /// When that stage was entered, when observed.
    pub entered_at: Option<DateTime<Utc>>,
    /// When it was left.
    pub left_at: Option<DateTime<Utc>>,
    /// `left_at − entered_at`, when both were observed (never a lower bound).
    pub duration_sec: Option<i64>,
    /// A **lower bound** on the stage's duration, when the row records a stage
    /// that did not complete: the item was still in it when it was observed,
    /// or it was cut short by something other than the stage finishing (#9328).
    ///
    /// Mutually exclusive with `duration_sec` — a row never carries both — and
    /// read only by `land-v2`'s Kaplan–Meier grids
    /// ([`JournalEntry::censored_sample`]); [`JournalEntry::history_sample`]
    /// ignores it, so no censored row can reach a v1 distribution.
    ///
    /// `#[serde(default)]`: rows written before #9328 simply have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub censored_sec: Option<i64>,
    /// The stage entered next, when known.
    pub next_stage: Option<Stage>,
    /// Judge verdict this row records (`pass`/`fail`), and its 1-based attempt.
    pub verdict: Option<String>,
    /// Attempt the verdict settled.
    pub attempt: Option<u32>,
    /// The sweep-outcome journal also records this duration.
    pub in_sweep: bool,
    /// How late the observation can be, in seconds (a pass interval for a
    /// listing diff, 0 for a bus event).
    pub resolution_sec: Option<i64>,
    /// Raw fields as observed (phase marker, labels, exit code, …).
    pub raw: serde_json::Value,
    /// The observing build.
    pub loom: Provenance,
}

impl JournalEntry {
    /// A row for `event` in `repo`, observed at `observed_at`, all else empty.
    #[must_use]
    pub fn new(event: &str, repo: &str, observed_at: DateTime<Utc>, loom: &Provenance) -> Self {
        JournalEntry {
            schema: JOURNAL_SCHEMA.to_string(),
            observed_at,
            event: event.to_string(),
            repo: repo.to_string(),
            issue: None,
            pr_number: None,
            sweep_id: None,
            stage: None,
            entered_at: None,
            left_at: None,
            duration_sec: None,
            censored_sec: None,
            next_stage: None,
            verdict: None,
            attempt: None,
            in_sweep: false,
            resolution_sec: None,
            raw: serde_json::Value::Object(serde_json::Map::new()),
            loom: loom.clone(),
        }
    }

    /// The stage sample this row contributes to history: a complete,
    /// non-negative duration that the sweep-outcome journal does not already
    /// carry. `host` is the host whose journal this is.
    #[must_use]
    pub fn history_sample(&self, host: &str) -> Option<StageSample> {
        if self.in_sweep {
            return None;
        }
        let (stage, duration_sec) = (self.stage?, self.duration_sec?);
        (duration_sec >= 0).then(|| StageSample {
            repo: self.repo.clone(),
            stage,
            duration_sec,
            observed_at: self.observed_at,
            source: SampleSource::StageJournal,
            host: host.to_string(),
            // #9420: a label-transition row records a stage boundary, not an
            // attempt, so there is no worked/unworked signal to carry.
            worked: None,
        })
    }

    /// The **right-censored** stage sample this row contributes (#9328): a
    /// non-negative `censored_sec` lower bound on a stage that never
    /// completed, from a journal the sweep-outcome one does not already carry.
    ///
    /// A row that carries a real `duration_sec` is never censored — the stage
    /// finished and [`Self::history_sample`] already has it.
    #[must_use]
    pub fn censored_sample(&self, host: &str) -> Option<StageSample> {
        if self.in_sweep || self.duration_sec.is_some() {
            return None;
        }
        let (stage, censored_sec) = (self.stage?, self.censored_sec?);
        (censored_sec >= 0).then(|| StageSample {
            repo: self.repo.clone(),
            stage,
            duration_sec: censored_sec,
            observed_at: self.observed_at,
            source: SampleSource::StageJournal,
            host: host.to_string(),
            worked: None,
        })
    }

    /// The Judge verdict this row contributes to history (external Judge
    /// only; in-sweep verdicts come from `sweep.outcome`).
    #[must_use]
    pub fn history_verdict(&self) -> Option<VerdictSample> {
        if self.in_sweep {
            return None;
        }
        let rejected = match self.verdict.as_deref()? {
            "fail" => true,
            "pass" => false,
            _ => return None,
        };
        Some(VerdictSample {
            repo: self.repo.clone(),
            attempt: self.attempt?,
            rejected,
            observed_at: self.observed_at,
        })
    }
}

/// Every stage-sample row one PR's `pr-latency` history (Issue #8923)
/// contributes for `eta backfill` (#9325): `review_wait` (PL1,
/// `loom:review-requested` → the next verdict, with the verdict itself so the
/// row also feeds [`StageSamples`]'s verdict-count branch probabilities),
/// `doctor` (PL4, `loom:changes-requested` → the next push) and `merge_wait`
/// (PL3, the `loom:pr` in force at merge → merge). Every `entered_at`/`left_at`
/// pair is an exact forge label-event timestamp, so `resolution_sec` is `0`.
///
/// This is a day-one baseline, not a full source (#9325's own scope note): it
/// has no way to tell whether a segment was already an in-sweep transition
/// the `sweep.outcome` journal recorded, so every row is `in_sweep: false` and
/// a PR driven end-to-end by `/loom:sweep` contributes to both journals.
/// `land-v1` is the only shipped heuristic that reads this journal
/// ([`SampleSource::StageJournal`]), so the exposure is bounded to it, and it
/// mirrors the same local/partial-view caveat #9343 already documents for
/// `sweep.outcome` history. A dedicated dedupe is future work.
///
/// # A label re-applied is not a stage re-entered
///
/// Concurrent agents routinely apply `loom:review-requested` twice seconds
/// apart, and a bare labeling count would read that as two review waits that
/// the same verdict answered — two near-duplicate rows that double-weight
/// one real observation and inflate the attempt number the verdict branch
/// probabilities are keyed on. [`entry_transitions`] counts *entries*
/// instead: a labeling only starts a segment when the label is not already
/// in force, where force is released by an `unlabeled` **or** by the event
/// that ends the stage (the verdict for `review_wait`, the push for
/// `doctor`). So a re-application is ignored while a genuine second lap
/// still counts — which also means two segments can never resolve to the
/// same verdict or push.
#[must_use]
pub fn entries_from_pr_history(
    h: &crate::pr_latency::PrHistory,
    repo: &str,
    loom: &Provenance,
) -> Vec<JournalEntry> {
    use crate::pr_latency::history::PrEvent;
    use crate::pr_latency::{APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED};

    /// The first verdict labeling strictly after `after`, with which label
    /// fired — `PrHistory::next_verdict_after` answers only the instant, and
    /// backfill needs the label too to route to `doctor` or `merge_wait`.
    fn next_verdict_label_after(
        h: &crate::pr_latency::PrHistory,
        after: DateTime<Utc>,
    ) -> Option<(DateTime<Utc>, &'static str)> {
        h.events
            .iter()
            .filter(|e| e.at() > after)
            .find_map(|e| match e {
                PrEvent::Labeled { label, at } if label == APPROVED => Some((*at, APPROVED)),
                PrEvent::Labeled { label, at } if label == CHANGES_REQUESTED => {
                    Some((*at, CHANGES_REQUESTED))
                }
                _ => None,
            })
    }

    let mut rows = Vec::new();
    let row_for = |stage: Stage, entered_at: DateTime<Utc>, left_at: DateTime<Utc>| {
        let mut row = JournalEntry::new("label.transition", repo, left_at, loom);
        row.pr_number = Some(h.number);
        row.stage = Some(stage);
        row.entered_at = Some(entered_at);
        row.left_at = Some(left_at);
        row.duration_sec = Some((left_at - entered_at).num_seconds().max(0));
        row.resolution_sec = Some(0);
        row
    };

    // PL1: each review *entry*, attempt-numbered in order, to the verdict
    // that answered it. The verdict is also what releases the stage, so two
    // entries can never share one: the next entry starts strictly after
    // this one's verdict.
    let verdict_clears = |e: &PrEvent| {
        matches!(e, PrEvent::Labeled { label, .. }
        if label == APPROVED || label == CHANGES_REQUESTED)
    };
    let mut attempt = 0_u32;
    for req in entry_transitions(h, REVIEW_REQUESTED, &verdict_clears) {
        let Some((verdict_at, label)) = next_verdict_label_after(h, req) else {
            continue;
        };
        attempt += 1;
        let mut row = row_for(Stage::ReviewWait, req, verdict_at);
        row.next_stage = Some(if label == APPROVED {
            Stage::MergeWait
        } else {
            Stage::Doctor
        });
        row.verdict = Some(if label == APPROVED { "pass" } else { "fail" }.to_string());
        row.attempt = Some(attempt);
        rows.push(row);
    }

    // PL4: each rejection entry, to the push that answered it. The push is
    // what ends a Doctor lap, so — as above — two entries cannot share one.
    let push_clears = |e: &PrEvent| matches!(e, PrEvent::Pushed { .. });
    for rejected in entry_transitions(h, CHANGES_REQUESTED, &push_clears) {
        let Some(push_at) = h.next_push_after(rejected) else {
            continue;
        };
        let mut row = row_for(Stage::Doctor, rejected, push_at);
        row.next_stage = Some(Stage::ReviewWait);
        rows.push(row);
    }

    // PL3: the approval in force at merge, to the merge.
    if let Some(merged_at) = h.merged_at {
        if let Some(approved_at) = h.last_labeled_before(APPROVED, merged_at) {
            let mut row = row_for(Stage::MergeWait, approved_at, merged_at);
            row.event = "pr.resolved".to_string();
            rows.push(row);
        }
    }

    rows
}

/// Every **right-censored** stage-sample row one PR's `pr-latency` history
/// contributes at `as_of` (#9328): the open segments
/// [`entries_from_pr_history`] drops on the floor.
///
/// Three, mirroring its three completed shapes:
///
/// - a `review_wait` entry with no verdict after it — the PR is sitting in
///   review right now;
/// - a `doctor` entry with no push after it — the rejection has not been
///   answered yet;
/// - an approval in force with no merge — `loom:pr` applied, still unmerged
///   (the merge-risk-hold shape this fleet produces constantly).
///
/// Each is censored at `as_of`, the instant the history was read: the stage
/// has lasted *at least* `as_of − entered_at`. Segments that started at or
/// after `as_of`, and PRs already merged or closed, contribute nothing.
///
/// This is where the bias `land-v1` carries actually comes from: every one of
/// these is a **slow** segment, and dropping them is what makes the observed
/// set read short. Kept as a separate function from
/// [`entries_from_pr_history`] so the completed rows every shipped heuristic
/// reads are byte-identical to before.
#[must_use]
pub fn censored_from_pr_history(
    h: &crate::pr_latency::PrHistory,
    repo: &str,
    as_of: DateTime<Utc>,
    loom: &Provenance,
) -> Vec<JournalEntry> {
    use crate::pr_latency::history::{PrEvent, PrState};
    use crate::pr_latency::{APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED};

    if h.state != PrState::Open {
        return Vec::new();
    }

    let mut rows = Vec::new();
    let mut open = |stage: Stage, entered_at: DateTime<Utc>| {
        if entered_at >= as_of {
            return;
        }
        let mut row = JournalEntry::new("stage.open", repo, as_of, loom);
        row.pr_number = Some(h.number);
        row.stage = Some(stage);
        row.entered_at = Some(entered_at);
        row.censored_sec = Some((as_of - entered_at).num_seconds().max(0));
        row.resolution_sec = Some(0);
        rows.push(row);
    };

    let verdict_clears = |e: &PrEvent| {
        matches!(e, PrEvent::Labeled { label, .. }
        if label == APPROVED || label == CHANGES_REQUESTED)
    };
    if let Some(req) = entry_transitions(h, REVIEW_REQUESTED, &verdict_clears)
        .into_iter()
        .next_back()
    {
        if h.next_verdict_after(req).is_none() {
            open(Stage::ReviewWait, req);
        }
    }

    let push_clears = |e: &PrEvent| matches!(e, PrEvent::Pushed { .. });
    if let Some(rejected) = entry_transitions(h, CHANGES_REQUESTED, &push_clears)
        .into_iter()
        .next_back()
    {
        if h.next_push_after(rejected).is_none() {
            open(Stage::Doctor, rejected);
        }
    }

    // An approval still in force on an unmerged PR: `merge_wait`, open.
    if h.merged_at.is_none() {
        if let Some(approved_at) = h.last_labeled_before(APPROVED, as_of) {
            let unlabeled = h.events.iter().any(|e| {
                matches!(e, PrEvent::Unlabeled { label, at } if label == APPROVED && *at > approved_at)
            });
            if !unlabeled {
                open(Stage::MergeWait, approved_at);
            }
        }
    }

    rows
}

/// Every instant `label` *entered* force on `h` — as opposed to
/// [`crate::pr_latency::PrHistory::labelings`]'s raw applications.
///
/// A second `labeled` while the label is already in force is the fleet
/// re-asserting a state, not re-entering it, and must not open a second
/// segment. Force is released by an explicit `unlabeled` **or** by any
/// event `clears` accepts — whatever actually ends the stage. That second
/// release matters because the fleet does not reliably remove
/// `loom:review-requested` when a verdict lands: without it a second review
/// lap on the same PR would be invisible, and with it
/// `verdict → re-request` reads as a new lap while `request → request`
/// (nothing in between) does not.
#[must_use]
pub fn entry_transitions(
    h: &crate::pr_latency::PrHistory,
    label: &str,
    clears: &dyn Fn(&crate::pr_latency::history::PrEvent) -> bool,
) -> Vec<DateTime<Utc>> {
    use crate::pr_latency::history::PrEvent;
    let mut present = false;
    let mut entries = Vec::new();
    for e in &h.events {
        match e {
            PrEvent::Labeled { label: l, at } if l == label => {
                if !present {
                    entries.push(*at);
                }
                present = true;
            }
            PrEvent::Unlabeled { label: l, .. } if l == label => present = false,
            other if clears(other) => present = false,
            _ => {}
        }
    }
    entries
}

/// The journal path for `workspace_root` (env override first).
#[must_use]
pub fn journal_path(workspace_root: &Path) -> PathBuf {
    match std::env::var(JOURNAL_PATH_ENV) {
        Ok(path) if !path.is_empty() => PathBuf::from(path),
        _ => workspace_root
            .join(".loom")
            .join("logs")
            .join(JOURNAL_FILENAME),
    }
}

fn rotated(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(JOURNAL_FILENAME);
    path.with_file_name(format!("{name}.1"))
}

/// Rotate `path` to `.1` when it is over the size cap or its first row is
/// older than the age cap (the sweep-outcome journal's thresholds).
fn rotate_if_needed(path: &Path) -> std::io::Result<()> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(());
    };
    let oversized = meta.len() >= crate::sweep_outcomes::MAX_JOURNAL_BYTES;
    let stale = !oversized
        && first_row(path).is_some_and(|row| {
            Utc::now() - row.observed_at
                > chrono::Duration::days(crate::sweep_outcomes::MAX_JOURNAL_AGE_DAYS)
        });
    if oversized || stale {
        std::fs::rename(path, rotated(path))?;
    }
    Ok(())
}

/// The journal's first (oldest) row, reading one line only.
fn first_row(path: &Path) -> Option<JournalEntry> {
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line).ok()?;
    serde_json::from_str(line.trim_end()).ok()
}

/// Append `entries` to the journal at `path`, one line each, creating the
/// directory and rotating first when needed.
pub fn append(path: &Path, entries: &[JournalEntry]) -> std::io::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    rotate_if_needed(path)?;
    let mut text = String::new();
    for entry in entries {
        if let Ok(line) = serde_json::to_string(entry) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(text.as_bytes())
}

/// Every parseable row of the journal at `path` and its `.1` rotation, oldest
/// generation first. Malformed lines are skipped.
#[must_use]
pub fn read(path: &Path) -> Vec<JournalEntry> {
    let mut rows = Vec::new();
    for file in [rotated(path), path.to_path_buf()] {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        rows.extend(
            text.lines()
                .filter_map(|line| serde_json::from_str::<JournalEntry>(line).ok()),
        );
    }
    rows
}

impl StageSamples {
    /// Add every history sample and verdict `entries` carry; `host` is the
    /// host whose journal they come from.
    pub fn push_journal(&mut self, entries: &[JournalEntry], host: &str) {
        for entry in entries {
            if let Some(sample) = entry.history_sample(host) {
                self.stages.push(sample);
            }
            if let Some(sample) = entry.censored_sample(host) {
                self.censored.push(sample);
            }
            if let Some(verdict) = entry.history_verdict() {
                self.verdicts.push(verdict);
            }
        }
    }
}
