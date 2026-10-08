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
//! # Progress under a small budget
//!
//! A timeline the budget interrupts is checkpointed on the pass
//! ([`PartialTimeline`]: the pages already read) and resumed at its next page,
//! so a timeline longer than the budget still completes over several cycles.
//! An uncovered pass re-reads its current listing page each cycle before any
//! timeline, so the budget has a floor of
//! [`super::config::MIN_FLEET_REFRESH_GAP_FILL_MAX_CALLS`] (`2`: one listing
//! page plus one timeline page), clamped at config parse: every cycle then
//! reads at least one timeline page, and the pass completes.
//!
//! # The raw-event phase
//!
//! The same cycle's raw-event sync (`observability::eta_fleet_refresh`,
//! issue events and pulls listings) follows [`EventsGate`]: a repo SigNoz
//! covered makes **no** raw-event read; an uncovered repo's event reads are
//! gap-fill, drawn from what its snapshot pass left of the same per-repo
//! budget; an unavailable SigNoz leaves them under the pass-kind budgets,
//! counted.
//!
//! A covered repo's raw cache is therefore not advanced, and star inputs
//! (`eta::star`, read from that cache) must not read the span it missed as
//! "unstarred". Each listing's cursor carries `synced_through`, the instant
//! the cache was last caught up with it (stamped whenever a refresh
//! completes); star coverage ends there, so a later cutoff reads as
//! **unknown**. A covered repo's stamps simply stop advancing; one with no
//! stamp yet (a cache from before #10520) is frozen at its newest cached
//! forge row of its own listing by [`freeze_raw_cache`] — never later than the
//! cache can vouch for.
//!
//! # Star coverage keeps advancing (#10746)
//!
//! Frozen coverage would, over time, turn every later star input unknown. So
//! a covered repo's two star listings (issue events and pulls) still get the
//! forge's cheap refresh: a completed backfill's ETag'd refresh, counted as
//! gap-fill and capped by the repo's budget, which stamps its listing on
//! completion. An interrupted refresh stamps nothing: coverage stays where it
//! was, unknown, never "unstarred".
//!
//! SigNoz never moves a stamp. A webhook row is dated by the Worker's receipt
//! time, whatever its later export time
//! ([`super::fleet_signoz_timeline_rows`]), so a successful query, even one
//! holding an old issue row, does not prove every change up to the listing
//! instant has been exported: a change received before the pass but exported
//! after it would be missing, and a stamp moved past it would read its cutoffs
//! as known. Without an explicit issue-label ingestion watermark (SigNoz has
//! none today), the forge alone establishes coverage. What SigNoz dates is
//! still recorded ([`record_signoz_stars`]): every issue star change of the
//! pass, as [`SOURCE_SIGNOZ`] raw rows, reconciled idempotently (a row's id is
//! its content, so a re-read never duplicates it), including a delayed export
//! dated at or before a listing's stamp, which repairs the cutoffs after it.
//!
//! Every forge read made while SigNoz history is configured — snapshot and
//! raw-event alike — is counted (`gap_fill_calls` on the repo's report and its
//! `eta.fleet_refresh` record, and the [`GAP_FILL_COUNTER`] facade counter),
//! persisted on the repo's refresh state ([`HistoryNote`]) and reported by
//! `eta doctor`: `0` when SigNoz covered the pass.
//!
//! # When an item is complete from SigNoz alone
//!
//! [`item_complete`]: the item's `opened` event is in the timeline (so its
//! whole label history since birth is), every label of the daemon's first
//! label set is explained by a recorded change, and it never carried
//! `loom:changes-requested`. The last rule is about pushes: SigNoz records
//! no push, and a Doctor lap ends at the push that answered a rejection
//! (`journal`'s PL4), so such a PR's history needs the forge timeline. An
//! item the daemon only ever saw as a label-set baseline (no `opened`), or
//! whose baseline carries a label no change dates, is gap-filled for the same
//! reason: those labels have no known time, and none is invented.
//!
//! # Every PR SigNoz knows is planned
//!
//! [`plan`] enumerates every PR item of the timeline, *including* one the
//! daemon only ever saw as label sets (a first baseline, or repeated
//! identical sets): such an item has no label or lifecycle event, but its
//! sets ([`ItemTimeline::label_sets`]) keep it, and their observations count
//! as touches. So the gap-fill decision sees it, rather than the covered pass
//! silently dropping it. Its listing labels are the latest set's
//! ([`ItemTimeline::current_labels`]).
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
use super::fleet_fetch::{history, timeline_url, ListedPr, PER_PAGE};
use super::fleet_refresh::{self, Pass, RepoReport, StopReason, Walk};
use super::fleet_signoz_refresh::{ClickhouseHttp, Limits, SignozRead};
use super::fleet_signoz_timeline::{Family, ItemTimeline, Timeline};
use super::fleet_signoz_timeline_rows::{walk, Lifecycle, Row, Target, TimelineHttp, Transition};
use super::star::{is_star_label, IssueStarChange};
use crate::forge_call_stats::ops::TIMELINE_READ;
use crate::pr_latency::timeline::parse_timeline_page;
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

/// A PR timeline a budget stop interrupted: the pages already read, kept on
/// the checkpointed [`Pass`] so the next cycle resumes at `next_page`. The
/// REST timeline is chronological, so pages already read stay valid. Dropped
/// when the pass reads a different PR first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialTimeline {
    pub number: u32,
    /// The timeline page to read next (1-based, `> 1`).
    pub next_page: u32,
    pub events: Vec<PrEvent>,
}

/// How the cycle's raw-event phase may read one repo (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventsGate {
    /// SigNoz history off: the pass-kind budgets only, nothing counted.
    Unbounded,
    /// SigNoz covered the window: no raw-event read.
    Skip,
    /// Counted as gap-fill. `Some(n)`: at most `n` more calls (the gap-fill
    /// budget the snapshot pass left); `None`: SigNoz unavailable, the
    /// pass-kind budgets only.
    GapFill(Option<u64>),
}

impl EventsGate {
    /// The gate for `report`'s repo under a per-repo `gap_budget`.
    #[must_use]
    pub fn for_repo(report: &RepoReport, gap_budget: u64) -> Self {
        match report.history {
            None => EventsGate::Unbounded,
            Some(HistorySource::Signoz | HistorySource::SignozGapFill) => EventsGate::Skip,
            Some(HistorySource::ForgeUncovered) => EventsGate::GapFill(Some(
                gap_budget.saturating_sub(report.gap_fill_calls.unwrap_or(0)),
            )),
            Some(HistorySource::ForgeUnavailable) => EventsGate::GapFill(None),
        }
    }

    /// The calls one listing sync may spend, given the pass-kind `left`.
    #[must_use]
    pub fn allowance(self, left: u64) -> u64 {
        match self {
            EventsGate::Skip => 0,
            EventsGate::GapFill(Some(cap)) => left.min(cap),
            EventsGate::Unbounded | EventsGate::GapFill(None) => left,
        }
    }

    /// Charge `spent` calls: shrink the cap and count them as gap-fill on
    /// `report` and the facade counter. A no-op unless gap-fill.
    pub fn charge(&mut self, report: &mut RepoReport, spent: u64) {
        let EventsGate::GapFill(cap) = self else {
            return;
        };
        if let Some(cap) = cap.as_mut() {
            *cap = cap.saturating_sub(spent);
        }
        report.gap_fill_calls = Some(report.gap_fill_calls.unwrap_or(0) + spent);
        for _ in 0..spent {
            crate::forge_call_stats::counters::bump(GAP_FILL_COUNTER);
        }
    }
}

/// The repo-wide listing (a [`super::star::listing_keys`] key) a cached forge
/// row belongs to, when it belongs to exactly one. The pulls listing is the
/// only source of `closing_ref` and `head_commit` rows; the issue-events
/// listing the only source of an issue's rows and of a PR's label and reopen
/// rows. A PR's `opened` / `closed` / `merged` rows come from both listings,
/// and a review or check run from a per-PR walk: none of those vouches for a
/// listing's own coverage, so they belong to no listing.
fn listing_of(event: &super::fleet_events::RawEvent) -> Option<String> {
    use super::fleet_events::{EventKind, ItemKind, SOURCE_FORGE};
    use super::fleet_events_forge::ForgeEndpoint;
    let endpoint = match (event.item_kind, event.kind) {
        (ItemKind::Pr, EventKind::ClosingRef | EventKind::HeadCommit) => ForgeEndpoint::Pulls,
        (ItemKind::Issue, _)
        | (ItemKind::Pr, EventKind::LabelAdded | EventKind::LabelRemoved | EventKind::Reopened) => {
            ForgeEndpoint::IssuesEvents
        }
        _ => return None,
    };
    Some(format!("{SOURCE_FORGE}:{}", endpoint.name()))
}

/// A covered repo's raw cache is not read this cycle: make sure each of its
/// repo-wide listings has a `synced_through` stamp, so star coverage ends
/// where the cache does (see the module docs). A listing already stamped
/// keeps its stamp; an unstamped one is stamped at the newest cached forge
/// row **of its own listing** ([`listing_of`]), the latest instant the cache
/// vouches for that listing (#10746: one newest row across the whole cache
/// stamped a listing later than its own rows could vouch for). A listing
/// with no qualifying row stays unstamped, so star inputs read it as
/// uncovered.
pub fn freeze_raw_cache(root: &Path, repo: &str) {
    use super::fleet_events::{self, EventsCursor, SOURCE_FORGE};
    let cursor_file = fleet_events::cursor_path(root, repo);
    let cursor = EventsCursor::read(&cursor_file, repo);
    let unstamped: Vec<String> = super::star::listing_keys()
        .into_iter()
        .filter(|k| {
            cursor
                .endpoints
                .get(k)
                .is_none_or(|e| e.synced_through.is_none())
        })
        .collect();
    if unstamped.is_empty() {
        return;
    }
    let mut newest: std::collections::BTreeMap<String, DateTime<Utc>> =
        std::collections::BTreeMap::new();
    for e in fleet_events::load_events(&fleet_events::events_path(root, repo))
        .iter()
        .filter(|e| e.source == SOURCE_FORGE)
    {
        if let Some(key) = listing_of(e) {
            let slot = newest.entry(key).or_insert(e.event_time);
            *slot = (*slot).max(e.event_time);
        }
    }
    for key in unstamped {
        let Some(at) = newest.get(&key).copied() else {
            continue;
        };
        if let Err(e) = fleet_events::mark_synced_through(&cursor_file, repo, &key, at) {
            log::warn!("eta fleet refresh: {repo}: could not freeze {key}'s coverage: {e}");
        }
    }
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
    /// The instant the timeline was built as knowable at.
    pub listed_at: Option<DateTime<Utc>>,
    /// The star changes the timeline dates on issues (see [`issue_stars`]).
    pub issue_stars: Vec<(IssueStarChange, String)>,
    /// Capacity backfill rows (#10959) for hourly instants the capacity log
    /// lacks, derived from the same walked rows; empty from [`plan`].
    pub capacity: Vec<super::capacity_log::CapacityRow>,
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
    /// Reachable, but not back to the window's start; the capacity backfill
    /// rows (#10959) are independent of that gate and still land.
    Uncovered(String, Vec<super::capacity_log::CapacityRow>),
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
    capacity_have: &std::collections::BTreeSet<DateTime<Utc>>,
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
    from_rows(repo, &rows, (listed_at, since), history_days, capacity_have)
}

/// [`load`]'s verdict over already-walked `rows`.
pub(super) fn from_rows(
    repo: &str,
    rows: &[Row],
    (listed_at, since): (DateTime<Utc>, DateTime<Utc>),
    history_days: i64,
    capacity_have: &std::collections::BTreeSet<DateTime<Utc>>,
) -> Load {
    let timeline = Timeline::build(rows, listed_at);
    let instants =
        super::capacity_log::backfill_instants(&timeline, listed_at, history_days, capacity_have);
    let capacity = super::capacity_log::backfill(rows, repo, &instants);
    for family in [Family::Labels, Family::Lifecycle] {
        if !timeline.coverage.covers(family, listed_at, history_days) {
            let first = timeline
                .coverage
                .first(family)
                .map_or_else(|| "none".to_string(), |at| at.to_rfc3339());
            return Load::Uncovered(
                format!(
                    "{family:?} rows start at {first}, after the {history_days}-day window's start"
                ),
                capacity,
            );
        }
    }
    let mut covered = plan(&timeline, listed_at, since);
    covered.capacity = capacity;
    Load::Covered(covered)
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
    Plan {
        candidates,
        listed_at: Some(cutoff),
        issue_stars: issue_stars(timeline),
        capacity: Vec::new(),
    }
}

/// Every star change `timeline` dates on an issue, by the one star-label
/// rule ([`is_star_label`]), each at the later of its time and its
/// observation (when it became knowable). A star label in an issue's
/// undated baseline set carries no addition time, so it is dated at the set's
/// observation: later than the truth, never earlier, so it cannot leak into a
/// row before the daemon saw it.
#[must_use]
pub fn issue_stars(timeline: &Timeline) -> Vec<(IssueStarChange, String)> {
    let mut out = Vec::new();
    for (key, item) in &timeline.items {
        if key.target != Target::Issue {
            continue;
        }
        for e in item.labels.iter().filter(|e| is_star_label(&e.label)) {
            let change = IssueStarChange {
                issue: key.number,
                at: e.at.max(e.observed_at),
                starred: e.transition == Transition::Added,
            };
            out.push((change, e.label.clone()));
        }
        if let Some((seen, _)) = item.label_sets.first() {
            for label in item
                .undated_baseline_labels()
                .into_iter()
                .filter(|l| is_star_label(l))
            {
                let change = IssueStarChange {
                    issue: key.number,
                    at: *seen,
                    starred: true,
                };
                out.push((change, label));
            }
        }
    }
    out.sort_by_key(|(c, label)| (c.at, c.issue, c.starred, label.clone()));
    out
}

/// `source` of a star change SigNoz dated (a raw-cache row the forge
/// listing never wrote).
pub const SOURCE_SIGNOZ: &str = "signoz";

/// Record the star changes a covered pass's SigNoz timeline dates on issues
/// (#10746) as [`SOURCE_SIGNOZ`] raw rows: every change knowable at the
/// pass's listing instant, whatever the listings' stamps. Returns how many
/// rows were new.
///
/// It never moves a stamp (see the module docs): a row SigNoz has not
/// exported yet may still carry a receipt time before the listing instant,
/// so only the forge refresh establishes coverage. Recording is idempotent
/// (a row's id is its content, and the log skips known ids), and a change
/// exported late, dated at or before a stamp, is recorded too: it repairs the
/// cutoffs after it rather than being discarded.
pub fn record_signoz_stars(root: &Path, repo: &str, plan: &Plan) -> usize {
    use super::fleet_events::{self, EventKind, EventLog, ItemKind, RawEvent};
    let Some(listed_at) = plan.listed_at else {
        return 0;
    };
    let rows: Vec<RawEvent> = plan
        .issue_stars
        .iter()
        .filter(|(c, _)| c.at <= listed_at)
        .map(|(c, label)| {
            RawEvent::new(
                repo,
                c.issue,
                ItemKind::Issue,
                if c.starred {
                    EventKind::LabelAdded
                } else {
                    EventKind::LabelRemoved
                },
                Some(label.clone()),
                c.at,
                SOURCE_SIGNOZ,
                0,
                listed_at,
            )
        })
        .collect();
    if rows.is_empty() {
        return 0;
    }
    match EventLog::open(&fleet_events::events_path(root, repo))
        .and_then(|mut log| log.append(&rows))
    {
        Ok(appended) => appended,
        Err(e) => {
            log::warn!("eta fleet refresh: {repo}: could not record SigNoz star changes: {e}");
            0
        }
    }
}

/// Whether `item` was touched (a change, or a daemon label set, became
/// knowable) at or after `since`.
fn touched_since(item: &ItemTimeline, since: DateTime<Utc>) -> bool {
    item.seen_at().any(|at| at >= since)
}

/// Whether SigNoz alone answers for `item` (see the module docs): its
/// `opened` is known, its baseline labels are all dated, and it never needed
/// a push to explain a Doctor lap.
#[must_use]
pub fn item_complete(item: &ItemTimeline) -> bool {
    let born = item.lifecycle.iter().any(|e| e.event == Lifecycle::Opened);
    let dated = item.undated_baseline_labels().is_empty();
    let rejected = item
        .labels
        .iter()
        .any(|e| e.label == CHANGES_REQUESTED && e.transition == Transition::Added);
    born && dated && !rejected
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
        .chain(item.label_sets.iter().map(|(seen, _)| *seen))
        .min();
    ListedPr {
        created_at: opened.or(earliest).unwrap_or(cutoff),
        state,
        merged_at: item.merged_at(),
        labels: item.current_labels(cutoff).into_iter().collect(),
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
///
/// The forge's `closed` / `reopened` events resolve the state when there is no
/// merge (#10746): a PR closed unmerged with no SigNoz close event would
/// otherwise stay as SigNoz listed it, open. Only events at or before
/// `cutoff` count (the listing instant), the last one deciding: `closed` is
/// [`PrState::Closed`], `reopened` is open. A merge outranks both (a merge
/// emits its own `closed` event), and with no close or reopen evidence the
/// SigNoz state stands: a missing event is never read as a close.
#[must_use]
pub fn gap_fill_history(
    number: u32,
    listed: &ListedPr,
    events: Vec<PrEvent>,
    cutoff: DateTime<Utc>,
) -> PrHistory {
    let mut listed = listed.clone();
    let merged = events.iter().rev().find_map(|e| match e {
        PrEvent::Merged { at } => Some(*at),
        _ => None,
    });
    if let Some(at) = merged {
        listed.state = PrState::Merged;
        listed.merged_at = Some(at);
    } else {
        let resolution = events
            .iter()
            .filter(|e| matches!(e, PrEvent::Closed { .. } | PrEvent::Reopened { .. }))
            .filter(|e| e.at() <= cutoff)
            .max_by_key(|e| e.at());
        match resolution {
            Some(PrEvent::Closed { .. }) => listed.state = PrState::Closed,
            Some(PrEvent::Reopened { .. }) => listed.state = PrState::Open,
            _ => {}
        }
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

/// Record `report`'s [`HistoryNote`] (at `now`) when SigNoz history is on.
pub fn note_report(root: &Path, report: &RepoReport, now: DateTime<Utc>) {
    if let (Some(source), Some(gap_fill_calls)) = (report.history, report.gap_fill_calls) {
        let history = HistoryNote {
            at: now,
            source,
            gap_fill_calls,
        };
        note(root, &report.repo, history);
    }
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
                freeze_raw_cache(self.root, &repo);
                record_signoz_stars(self.root, &repo, &plan);
                if let Err(e) = super::capacity_log::append(self.root, &plan.capacity) {
                    log::warn!("eta fleet refresh: {repo}: could not log capacity backfill: {e}");
                }
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
            Load::Uncovered(why, capacity) => {
                if let Err(e) = super::capacity_log::append(self.root, &capacity) {
                    log::warn!("eta fleet refresh: {repo}: could not log capacity backfill: {e}");
                }
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

    /// Every page of PR `number`'s timeline, resuming `pass.partial` when it
    /// is this PR's: `Ok(None)` when a page did not parse (an incomplete
    /// timeline). A stop after page 1 keeps the pages read on `pass`.
    pub(super) fn timeline(
        &mut self,
        pass: &mut Pass,
        number: u32,
    ) -> Result<Option<Vec<PrEvent>>, StopReason> {
        let repo = self.target.repo.clone();
        let mut partial = match pass.partial.take() {
            Some(p) if p.number == number => p,
            _ => PartialTimeline {
                number,
                next_page: 1,
                events: Vec::new(),
            },
        };
        loop {
            let url = timeline_url(&repo, number, partial.next_page);
            let answer = match self.call(&url, None, TIMELINE_READ) {
                Ok(answer) => answer,
                Err(stop) => {
                    if partial.next_page > 1 {
                        pass.partial = Some(partial);
                    }
                    return Err(stop);
                }
            };
            let Some((page_events, raw)) = parse_timeline_page(answer.body.as_bytes()) else {
                return Ok(None);
            };
            partial.events.extend(page_events);
            if raw < PER_PAGE {
                return Ok(Some(partial.events));
            }
            partial.next_page += 1;
        }
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
                    match self.timeline(pass, number) {
                        Ok(Some(events)) => {
                            let h =
                                gap_fill_history(number, &candidate.listed, events, pass.listed_at);
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
