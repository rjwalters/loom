//! SigNoz as the primary source of fleet-refresh history (#10520, slice 3 of
//! #10511), with forge reads only filling gaps.
//!
//! # What changes
//!
//! With `autonomous.eta.fleetRefresh.signoz` enabled and `historyPrimary`
//! opted in (default `false`, so turning SigNoz on never silently switches
//! the fit's input source), each repo's pass first walks its
//! SigNoz timeline ([`super::fleet_signoz_timeline_rows::walk`]) and builds
//! a [`Timeline`] as knowable at the pass's listing instant `L`. Three
//! outcomes ([`Load`]):
//!
//! - **Covered** — both the label and the lifecycle families reach back to
//!   `L − backfillDays` ([`super::fleet_signoz_timeline::Coverage::covers`]).
//!   The pass makes **no listing call** (the refresh `304` shortcut is skipped,
//!   not bypassed) and merges every PR touched since the pass's `S` from
//!   SigNoz. Only the *items* SigNoz cannot answer alone ([`item_complete`])
//!   are gap-filled with one ETag'd reader-App timeline read each.
//! - **Uncovered** — SigNoz is reachable but does not reach back far enough:
//!   the whole pass falls back to the forge walk. Those reads are gap-fill
//!   too.
//! - **Unavailable** — the walk failed (backend down, invalid page, page
//!   ceiling). A partial walk is never treated as covered: the pass degrades
//!   to the pre-#10520 forge behaviour, bounded by the existing budgets only.
//!
//! Gap-fill reads (covered and uncovered) are bounded by a hard per-repo,
//! per-cycle budget, `fleetRefresh.gapFillMaxCallsPerPass`, on top of the
//! pass-kind budgets. When it is spent the pass stops `budget`, logs it and
//! checkpoints exactly as a pass-kind budget stop does, so the next cycle
//! resumes it and nothing overruns.
//!
//! Every forge read made while SigNoz history is configured is counted
//! (`gap_fill_calls` on the repo's report and its `eta.fleet_refresh` record,
//! and the [`GAP_FILL_COUNTER`] facade counter), persisted on the repo's
//! refresh state ([`HistoryNote`]) and reported by `eta doctor`: `0` when
//! SigNoz covered the pass.
//!
//! # When an item is complete from SigNoz alone
//!
//! [`item_complete`]: the item's `opened` event is in the timeline (so its
//! whole label history since birth is), and it never carried
//! `loom:changes-requested`. The second rule is about pushes: SigNoz records
//! no push, and a Doctor lap ends at the push that answered a rejection
//! (`journal`'s PL4), so such a PR's history needs the forge timeline. An
//! item the daemon only ever saw as a label-set baseline (no `opened`) is
//! gap-filled for the same reason: its first labels have no known time.
//!
//! A gap-filled item takes its *events* from the forge timeline and its
//! listing facts (state, merge instant) from SigNoz, with the forge's own
//! `merged` event winning the merge instant.
//!
//! # Parity and the polling tolerance
//!
//! A SigNoz-sourced history equals the forge-sourced one in its label
//! transitions and merge instant, up to the time each event became knowable
//! to its producer:
//!
//! - **Webhook-sourced events** carry the loom-ui Worker's receipt time:
//!   within [`super::fleet_signoz_timeline::MATCH_SLACK_SEC`] of the forge's
//!   own time.
//! - **Daemon-sourced events** with no webhook partner are dated by the
//!   daemon's listing diff, which runs on the collector's 5-minute pass: up
//!   to [`DAEMON_POLL_TOLERANCE_SEC`] late (one pass, plus one missed pass).
//!   Webhook events are not polled, so this slack never applies to them.
//!
//! `eta/tests/fleet_signoz_history.rs` pins both bounds on the recorded
//! fixture. Two further forge-path facts are deliberately mirrored, not
//! improved: `closed_at` stays `None` (the forge listing path never sets it
//! either), and `created_at` is the `opened` instant (unused by the snapshot
//! derivation).

use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::config::FleetSignozConfig;
use super::fleet::FleetSnapshot;
use super::fleet_fetch::{history, ListedPr};
use super::fleet_refresh::{self, Pass, StopReason, Walk};
use super::fleet_signoz_refresh::{ClickhouseHttp, Limits, SignozRead};
use super::fleet_signoz_timeline::{Family, ItemTimeline, Timeline};
use super::fleet_signoz_timeline_rows::{walk, Lifecycle, Target, TimelineHttp, Transition};
use crate::pr_latency::{PrEvent, PrHistory, PrState, CHANGES_REQUESTED};

/// How far before the history window the SigNoz walk starts, so
/// [`super::fleet_signoz_timeline::Coverage::covers`] can see a row at or
/// before the window's start (the query never returns rows older than the
/// walk's own start).
pub const COVERAGE_PROBE_DAYS: i64 = 7;

/// How late a daemon-sourced event's time may be against the forge's: one
/// collector pass (300 s) plus one missed pass. See the module docs.
pub const DAEMON_POLL_TOLERANCE_SEC: i64 = 600;

/// The `forge_call_stats` facade counter every gap-fill read bumps.
pub const GAP_FILL_COUNTER: &str = "eta_fleet_refresh.gap_fill";

/// Where a repo's pass took its history from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistorySource {
    /// SigNoz covered the window and every item: no forge read.
    Signoz,
    /// SigNoz covered the window; some items were gap-filled from the forge.
    SignozGapFill,
    /// SigNoz does not reach back far enough: the forge walk, gap-fill
    /// budgeted.
    ForgeUncovered,
    /// The SigNoz walk failed: the forge walk under the pass-kind budgets.
    ForgeUnavailable,
}

impl HistorySource {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            HistorySource::Signoz => "signoz",
            HistorySource::SignozGapFill => "signoz_gap_fill",
            HistorySource::ForgeUncovered => "forge_uncovered",
            HistorySource::ForgeUnavailable => "forge_unavailable",
        }
    }
}

/// A repo's last SigNoz-primary pass, persisted on its refresh state for
/// `eta doctor` (which never calls the forge or SigNoz).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryNote {
    /// The cycle's `now`.
    pub at: DateTime<Utc>,
    pub source: HistorySource,
    /// Forge reads the repo made that cycle.
    pub gap_fill_calls: u64,
}

/// One PR the pass merges.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub number: u32,
    /// Its listing facts, from SigNoz.
    pub listed: ListedPr,
    /// `Some` when SigNoz alone answers for it; `None` means gap-fill.
    pub history: Option<PrHistory>,
}

/// A covered window's work: every PR touched since `S`, ascending.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub candidates: Vec<Candidate>,
}

impl Plan {
    /// Candidates that need a forge read.
    #[must_use]
    pub fn gap_fills(&self) -> usize {
        self.candidates
            .iter()
            .filter(|c| c.history.is_none())
            .count()
    }
}

/// What the SigNoz walk said about one pass.
#[derive(Debug, Clone)]
pub enum Load {
    Covered(Plan),
    /// Reachable, but not back to the window's start.
    Uncovered(String),
    /// The walk failed; never treated as covered.
    Unavailable(String),
}

/// Walk `repo`'s SigNoz timeline for a pass listing at `listed_at` that
/// merges PRs touched since `since`, over a history `history_days` deep.
pub fn load(
    repo: &str,
    reader: &mut dyn SignozRead,
    limits: Limits,
    (listed_at, since): (DateTime<Utc>, DateTime<Utc>),
    history_days: i64,
) -> Load {
    let start = listed_at - Duration::days(history_days + COVERAGE_PROBE_DAYS);
    let rows = match walk(repo, reader, start, listed_at, limits) {
        Ok((rows, _)) => rows,
        Err(report) => {
            return Load::Unavailable(format!(
                "{:?}: {}",
                report.stop,
                report.detail.as_deref().unwrap_or("no detail")
            ))
        }
    };
    let timeline = Timeline::build(&rows, listed_at);
    for family in [Family::Labels, Family::Lifecycle] {
        if !timeline.coverage.covers(family, listed_at, history_days) {
            let first = timeline
                .coverage
                .first(family)
                .map_or_else(|| "none".to_string(), |at| at.to_rfc3339());
            return Load::Uncovered(format!(
                "{family:?} rows start at {first}, after the {history_days}-day window's start"
            ));
        }
    }
    Load::Covered(plan(&timeline, listed_at, since))
}

/// Every PR of `timeline` touched since `since`, ascending.
#[must_use]
pub fn plan(timeline: &Timeline, cutoff: DateTime<Utc>, since: DateTime<Utc>) -> Plan {
    let candidates = timeline
        .items
        .iter()
        .filter(|(key, item)| key.target == Target::Pr && touched_since(item, since))
        .map(|(key, item)| Candidate {
            number: key.number,
            listed: listed_pr(item, cutoff),
            history: item_complete(item).then(|| pr_history(key.number, item, cutoff)),
        })
        .collect();
    Plan { candidates }
}

fn touched_since(item: &ItemTimeline, since: DateTime<Utc>) -> bool {
    let label = item.labels.iter().map(|e| e.at.max(e.observed_at));
    let life = item.lifecycle.iter().map(|e| e.at.max(e.observed_at));
    label.chain(life).any(|at| at >= since)
}

/// Whether SigNoz alone answers for `item` (see the module docs): its
/// `opened` is known, and it never needed a push to explain a Doctor lap.
#[must_use]
pub fn item_complete(item: &ItemTimeline) -> bool {
    let born = item.lifecycle.iter().any(|e| e.event == Lifecycle::Opened);
    let rejected = item
        .labels
        .iter()
        .any(|e| e.label == CHANGES_REQUESTED && e.transition == Transition::Added);
    born && !rejected
}

/// The listing facts SigNoz knows for `item` at `cutoff`.
#[must_use]
pub fn listed_pr(item: &ItemTimeline, cutoff: DateTime<Utc>) -> ListedPr {
    let state = match item.resolution().map(|e| e.event) {
        Some(Lifecycle::Merged) => PrState::Merged,
        Some(Lifecycle::Closed) => PrState::Closed,
        _ => PrState::Open,
    };
    let opened = item
        .lifecycle
        .iter()
        .find(|e| e.event == Lifecycle::Opened)
        .map(|e| e.at);
    let earliest = item
        .labels
        .iter()
        .map(|e| e.at)
        .chain(item.lifecycle.iter().map(|e| e.at))
        .min();
    ListedPr {
        created_at: opened.or(earliest).unwrap_or(cutoff),
        state,
        merged_at: item.merged_at(),
        labels: item.labels_at(cutoff).into_iter().collect(),
    }
}

/// `item` as the [`PrHistory`] [`super::fleet_fetch::history`] builds from
/// the forge: label events, and the merge as a [`PrEvent::Merged`].
#[must_use]
pub fn pr_history(number: u32, item: &ItemTimeline, cutoff: DateTime<Utc>) -> PrHistory {
    let listed = listed_pr(item, cutoff);
    let mut events: Vec<PrEvent> = item
        .labels
        .iter()
        .map(|e| match e.transition {
            Transition::Added => PrEvent::Labeled {
                label: e.label.clone(),
                at: e.at,
            },
            Transition::Removed => PrEvent::Unlabeled {
                label: e.label.clone(),
                at: e.at,
            },
        })
        .collect();
    if let Some(at) = listed.merged_at {
        events.push(PrEvent::Merged { at });
    }
    history(number, &listed, events, true)
}

/// A gap-filled PR: the forge timeline's events, SigNoz's listing facts,
/// the forge's own `merged` event winning the merge instant.
#[must_use]
pub fn gap_fill_history(number: u32, listed: &ListedPr, events: Vec<PrEvent>) -> PrHistory {
    let mut listed = listed.clone();
    let merged = events.iter().rev().find_map(|e| match e {
        PrEvent::Merged { at } => Some(*at),
        _ => None,
    });
    if let Some(at) = merged {
        listed.state = PrState::Merged;
        listed.merged_at = Some(at);
    }
    history(number, &listed, events, true)
}

/// The production timeline reader for `config`, when SigNoz history is on.
#[must_use]
pub fn reader(config: &FleetSignozConfig) -> Option<(TimelineHttp, Limits)> {
    if !(config.enabled && config.history_primary) {
        return None;
    }
    let endpoint = config.endpoint.clone()?;
    let http = ClickhouseHttp {
        endpoint,
        user: config.user.clone(),
        credential_file: config.credential_file.clone(),
        timeout: std::time::Duration::from_secs(60),
    };
    let limits = Limits {
        page_size: config.page_size,
        max_pages: config.max_pages,
    };
    Some((TimelineHttp(http), limits))
}

/// Record `note` on `repo`'s refresh state, when it has one.
pub fn note(root: &Path, repo: &str, note: HistoryNote) {
    let path = fleet_refresh::state_path(root, repo);
    let Some(mut state) = fleet_refresh::read_state(&path) else {
        return;
    };
    state.history = Some(note);
    if let Err(e) = fleet_refresh::write_state(&path, &state) {
        log::warn!("eta fleet refresh: {repo}: could not record its history source: {e}");
    }
}

impl Walk<'_> {
    /// Apply one pass's [`Load`]: set the report's source, the gap-fill
    /// budget and counter, and return the plan when the window is covered.
    pub(super) fn apply(&mut self, load: Option<Load>, gap_budget: u64) -> Option<Plan> {
        let load = load?;
        let repo = self.target.repo.clone();
        self.report.gap_fill_calls = Some(0);
        match load {
            Load::Covered(plan) => {
                self.report.history = Some(HistorySource::Signoz);
                self.gap_left = Some(gap_budget);
                if plan.gap_fills() > 0 {
                    log::info!(
                        "eta fleet refresh: {repo}: SigNoz covers the window; {} of {} PR(s) \
                         need a forge gap-fill",
                        plan.gap_fills(),
                        plan.candidates.len()
                    );
                }
                Some(plan)
            }
            Load::Uncovered(why) => {
                log::info!(
                    "eta fleet refresh: {repo}: SigNoz does not cover the window ({why}); \
                     forge gap-fill, at most {gap_budget} call(s) this cycle"
                );
                self.report.history = Some(HistorySource::ForgeUncovered);
                self.gap_left = Some(gap_budget);
                None
            }
            Load::Unavailable(why) => {
                log::info!(
                    "eta fleet refresh: {repo}: SigNoz timeline unavailable ({why}); using the \
                     forge under the pass budgets"
                );
                self.report.history = Some(HistorySource::ForgeUnavailable);
                None
            }
        }
    }

    /// Charge one forge read against the gap-fill budget (#10520), before
    /// [`Walk::call`] spends it: `Err(Budget)` (logged; the caller
    /// checkpoints) when the budget is spent, else count it as gap-fill when
    /// SigNoz history is on. A no-op with SigNoz history off.
    pub(super) fn charge_gap_fill(&mut self) -> Result<(), StopReason> {
        if self.gap_left == Some(0) {
            log::info!(
                "eta fleet refresh: {}: gap-fill budget spent ({} call(s)); checkpointing",
                self.target.repo,
                self.report.gap_fill_calls.unwrap_or(0)
            );
            return Err(StopReason::Budget);
        }
        if let Some(left) = self.gap_left.as_mut() {
            *left -= 1;
        }
        if let Some(calls) = self.report.gap_fill_calls.as_mut() {
            *calls += 1;
            crate::forge_call_stats::counters::bump(GAP_FILL_COUNTER);
        }
        Ok(())
    }

    /// Merge a covered window's plan: SigNoz histories with no call, the
    /// rest through [`Walk::timeline`] (gap-fill, budgeted in `call`).
    pub(super) fn walk_signoz(
        &mut self,
        pass: &mut Pass,
        staging: &mut FleetSnapshot,
        plan: &Plan,
    ) -> StopReason {
        for candidate in &plan.candidates {
            let number = candidate.number;
            if pass.done.binary_search(&number).is_ok() {
                continue;
            }
            match &candidate.history {
                Some(h) => staging.merge(std::slice::from_ref(h), pass.listed_at),
                None => {
                    self.report.history = Some(HistorySource::SignozGapFill);
                    match self.timeline(number) {
                        Ok(Some(events)) => {
                            let h = gap_fill_history(number, &candidate.listed, events);
                            staging.merge(&[h], pass.listed_at);
                        }
                        Ok(None) => self.report.timelines_incomplete += 1,
                        Err(stop) => return stop,
                    }
                }
            }
            self.report.prs_read += 1;
            if let Err(at) = pass.done.binary_search(&number) {
                pass.done.insert(at, number);
            }
        }
        StopReason::Complete
    }
}
