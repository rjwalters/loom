//! The fleet snapshot backfill / refresh state machine (#10263).
//!
//! # Why
//!
//! The daily fit (#10245) and `historyScope = augment` read only the cached
//! fleet snapshots ([`super::fleet::load_all`]). Before this, only the manual
//! `eta fleet backfill|refresh` CLI wrote them, so on a fleet host they were
//! missing or stale and the fit had nothing to train on. This module is the
//! in-process derivation the daemon's `observability::eta_fleet_refresh` task
//! drives once per cycle, off every tick.
//!
//! # Shape
//!
//! [`run_cycle`] has no clock (`now` is passed in) and no forge of its own: it
//! reads through a [`ForgeRead`] and writes only files under
//! [`refresh_dir`] and the published snapshot. Per repo it runs one **pass**:
//!
//! - **backfill** — no published snapshot, no state file (a CLI-made snapshot
//!   covers ~3–5 days at its default `--limit`), coverage shallower than
//!   `backfillDays`, or a [`DERIVATION_REV`] bump. Staging starts empty and
//!   reaches back `backfillDays` from the listing instant `L`; the published
//!   snapshot stays in service until promotion.
//! - **refresh** — otherwise. Staging starts as the published snapshot and
//!   reaches back to `watermark −` [`WATERMARK_SLACK_SEC`]. Page 1 goes first
//!   with `If-None-Match`; a `304` costs one call, advances `as_of` and the
//!   watermark, and stops `not_modified`.
//!
//! # Resume — the in-process exit 75
//!
//! Every merge in a pass uses `as_of = L`, so staging is one consistent
//! snapshot as of `L` however many cycles the pass spans. After each listing
//! page, and at every stop, staging is written (atomically) **then** the
//! state; the next cycle continues the same pass from `next_page` with the same
//! `L` and `done`. Newest-first ordering makes a page index a safe resume
//! point: new activity only pushes older rows later, so a resumed walk can
//! re-see a row (skipped via `done`) but never miss one — and anything updated
//! after `L` is re-read by the next refresh, which starts from `L − 10 min`.
//! A crash between the two writes re-reads a few PRs; `merge` is idempotent
//! per PR. On load, a state naming a pass whose staging is unreadable restarts
//! the pass, and a staging file with no pass is deleted.
//!
//! # Budgets and stops
//!
//! Every request counts, `304`s and errors included, against the host-wide
//! budget of its pass kind. A response reporting fewer than `reserve` core
//! calls remaining is kept, then every further repo on that reader
//! installation — App and repo owner, the bucket the header reports (#10329)
//! — is skipped (`reserve`). A rate limit ends the cycle; a coverage gap or other
//! forge error ends that repo; an open breaker or a shutdown ends the cycle
//! before the next call. A due backfill skipped before its first call is
//! recorded as in progress from that cycle ([`pend_backfill`], #10292), so it
//! holds the fit like any other.
//!
//! # SigNoz first (#10520)
//!
//! [`run_cycle_with`] given a SigNoz timeline reader takes each pass's history
//! from SigNoz and reads the forge only to fill gaps, under one more budget
//! ([`Budgets::gap_fill`]); see [`super::fleet_signoz_history`].

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::fleet::{self, FleetSnapshot};
use super::fleet_fetch::{
    history, listing_url, parse_listing, ForgeRead, Installation, NoReader, Read, ReadFailure,
    Reader, RepoTarget, PER_PAGE,
};
use super::fleet_signoz_history::{self as signoz_history, HistoryNote, HistorySource, Plan};
use super::fleet_signoz_refresh::{Limits, SignozRead};
use crate::forge_call_stats::ops::ISSUE_LIST;
use crate::forge_call_stats::ForgeOp;

/// Schema tag of a state file.
pub const STATE_SCHEMA: &str = "eta-fleet-refresh-state/v1";

/// The derivation revision a completed backfill records. Bump it when the
/// snapshot derivation changes (the kind of change #10245's `flag_changes`
/// is): every repo then gets a fresh backfill on its next cycle.
///
/// 2 (#10500): `merges` records every forge merge, labelled or not.
pub const DERIVATION_REV: u32 = 2;

/// Fixed slack subtracted from the watermark when a refresh chooses how far
/// back to read: clock skew and listing lag.
pub const WATERMARK_SLACK_SEC: i64 = 600;

/// How long an in-progress backfill holds the fit, counted from the pass's
/// own listing instant.
pub const FIT_HOLD_HOURS: i64 = 6;

/// Which walk a pass performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassKind {
    Backfill,
    Refresh,
}

impl PassKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PassKind::Backfill => "backfill",
            PassKind::Refresh => "refresh",
        }
    }
}

/// An in-progress pass, persisted between cycles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pass {
    pub kind: PassKind,
    /// `L`: the instant before the pass's first listing call. Every merge in
    /// the pass uses it as `as_of`.
    pub listed_at: DateTime<Utc>,
    /// `S`: rows updated before this end the walk.
    pub since: DateTime<Utc>,
    /// The listing page to read next (1-based).
    pub next_page: u32,
    /// PRs already read in this pass, ascending.
    pub done: Vec<u32>,
    /// Page 1's ETag as this pass read it — the next refresh's validator once
    /// the pass completes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_etag: Option<String>,
    /// A timeline a budget interrupted, resumed next cycle (#10520).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<signoz_history::PartialTimeline>,
}

/// Why a repo's cycle ended. Also the record's `stop_reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Complete,
    NotModified,
    Budget,
    Reserve,
    RateLimited,
    Coverage,
    BreakerOpen,
    Backoff,
    NoReader,
    UnsupportedForge,
    ForgeError,
    WriteError,
    Shutdown,
}

impl StopReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::Complete => "complete",
            StopReason::NotModified => "not_modified",
            StopReason::Budget => "budget",
            StopReason::Reserve => "reserve",
            StopReason::RateLimited => "rate_limited",
            StopReason::Coverage => "coverage",
            StopReason::BreakerOpen => "breaker_open",
            StopReason::Backoff => "backoff",
            StopReason::NoReader => "no_reader",
            StopReason::UnsupportedForge => "unsupported_forge",
            StopReason::ForgeError => "forge_error",
            StopReason::WriteError => "write_error",
            StopReason::Shutdown => "shutdown",
        }
    }
}

/// When and why a repo last stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastStop {
    pub at: DateTime<Utc>,
    pub reason: StopReason,
}

/// `refresh/<slug>.json`: one repo's refresh bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshState {
    /// Always [`STATE_SCHEMA`].
    pub schema: String,
    /// `owner/repo`.
    pub repo: String,
    /// `L` of the last completed pass (or `304`).
    pub watermark: Option<DateTime<Utc>>,
    /// Page 1's validator as of the watermark.
    pub listing_etag: Option<String>,
    /// `S` of the last completed backfill: the published snapshot reaches
    /// back at least this far.
    pub covered_since: Option<DateTime<Utc>>,
    /// [`DERIVATION_REV`] of the last completed backfill.
    pub derivation_rev: u32,
    /// The pass in progress, if any.
    pub pass: Option<Pass>,
    /// The last stop.
    pub last_stop: Option<LastStop>,
    /// The last SigNoz-primary pass's source and gap-fill count (#10520).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<HistoryNote>,
}

impl RefreshState {
    #[must_use]
    pub fn new(repo: &str) -> Self {
        RefreshState {
            schema: STATE_SCHEMA.to_string(),
            repo: repo.to_string(),
            watermark: None,
            listing_etag: None,
            covered_since: None,
            derivation_rev: 0,
            pass: None,
            last_stop: None,
            history: None,
        }
    }
}

/// Where state and staging files live: a `refresh/` subdirectory of the
/// snapshot cache, which [`fleet::load_all`] never lists.
#[must_use]
pub fn refresh_dir(workspace_root: &Path) -> PathBuf {
    fleet::snapshot_dir(workspace_root).join("refresh")
}

/// `refresh/<slug>.json`.
#[must_use]
pub fn state_path(workspace_root: &Path, repo: &str) -> PathBuf {
    refresh_dir(workspace_root).join(format!("{}.json", fleet::snapshot_slug(repo)))
}

/// `refresh/<slug>.staging.json`: the snapshot a pass is building.
#[must_use]
pub fn staging_path(workspace_root: &Path, repo: &str) -> PathBuf {
    refresh_dir(workspace_root).join(format!("{}.staging.json", fleet::snapshot_slug(repo)))
}

/// The state at `path`, or `None` when absent, unreadable or of another
/// schema.
#[must_use]
pub fn read_state(path: &Path) -> Option<RefreshState> {
    let text = std::fs::read_to_string(path).ok()?;
    let state: RefreshState = serde_json::from_str(&text).ok()?;
    (state.schema == STATE_SCHEMA).then_some(state)
}

/// Write `state` atomically (temp file + rename).
///
/// # Errors
///
/// The directory could not be created or the write/rename failed.
pub fn write_state(path: &Path, state: &RefreshState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{text}\n"))?;
    std::fs::rename(&tmp, path)
}

/// The cycle's budgets, from `autonomous.eta.fleetRefresh`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    /// Calls for refresh passes, host-wide.
    pub refresh: u64,
    /// Calls for backfill passes, host-wide.
    pub backfill: u64,
    /// The reserve floor.
    pub reserve: u64,
    /// Backfill depth, days.
    pub backfill_days: i64,
    /// Gap-fill calls per repo per cycle when SigNoz history is on (#10520).
    pub gap_fill: u64,
}

/// One repo's cycle — the `eta.fleet_refresh` record's fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoReport {
    pub repo: String,
    pub pass: Option<PassKind>,
    pub stop: StopReason,
    pub promoted: bool,
    /// PRs whose timeline was read this cycle (complete or not).
    pub prs_read: u64,
    /// `done.len()` of the pass at the end of the cycle.
    pub pass_done: u64,
    pub timelines_incomplete: u64,
    /// Published samples after minus before.
    pub samples_added: i64,
    pub forge_calls: u64,
    pub not_modified_calls: u64,
    pub ratelimit_remaining_min: Option<u64>,
    pub reader_app: Option<String>,
    /// The published snapshot after the cycle.
    pub snapshot_id: Option<String>,
    pub as_of: Option<DateTime<Utc>>,
    /// Raw event rows appended (#10250 cache), when that sync ran.
    pub raw_events_added: Option<u64>,
    /// Forge reads made while SigNoz history is on (#10520): `Some(0)` when
    /// SigNoz covered the pass, `None` when it is off.
    pub gap_fill_calls: Option<u64>,
    /// Where the pass took its history from, when SigNoz history is on.
    pub history: Option<HistorySource>,
    /// Wall time spent on the repo. Telemetry only: measured with a
    /// monotonic `Instant`, it never reaches a file or a decision.
    pub duration_ms: u64,
}

impl RepoReport {
    fn skipped(repo: &str, pass: Option<PassKind>, stop: StopReason) -> Self {
        RepoReport {
            repo: repo.to_string(),
            pass,
            stop,
            promoted: false,
            prs_read: 0,
            pass_done: 0,
            timelines_incomplete: 0,
            samples_added: 0,
            forge_calls: 0,
            not_modified_calls: 0,
            ratelimit_remaining_min: None,
            reader_app: None,
            snapshot_id: None,
            as_of: None,
            raw_events_added: None,
            gap_fill_calls: None,
            history: None,
            duration_ms: 0,
        }
    }
}

/// What one cycle did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CycleReport {
    /// One per target, in the order they ran.
    pub repos: Vec<RepoReport>,
    /// Calls spent against the refresh budget.
    pub refresh_calls: u64,
    /// Calls spent against the backfill budget.
    pub backfill_calls: u64,
    /// A reader App was rate limited; the forge's reset, when it said.
    pub rate_limited: Option<Option<i64>>,
    /// The earliest `L` of any backfill still in progress after the cycle —
    /// what [`fit_held`] reads.
    pub backfill_in_progress_since: Option<DateTime<Utc>>,
    /// Calls left in each budget after the cycle `(refresh, backfill)`.
    pub remaining: (u64, u64),
}

/// The fit hold rule: while a backfill is in progress, skip the fit until
/// [`FIT_HOLD_HOURS`] after that backfill began, so a fresh host's first fit
/// is not trained on only the repos that happened to finish first.
#[must_use]
pub fn fit_held(backfill_since: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    backfill_since.is_some_and(|since| now < since + Duration::hours(FIT_HOLD_HOURS))
}

/// Which pass a repo with no pass in progress gets.
#[must_use]
pub fn choose_pass(
    state: Option<&RefreshState>,
    published: bool,
    now: DateTime<Utc>,
    backfill_days: i64,
) -> PassKind {
    let Some(state) = state else {
        return PassKind::Backfill;
    };
    let shallow = state
        .covered_since
        .is_none_or(|c| c > now - Duration::days(backfill_days));
    if !published || state.watermark.is_none() || state.derivation_rev < DERIVATION_REV || shallow {
        PassKind::Backfill
    } else {
        PassKind::Refresh
    }
}

/// Crash recovery for one repo's files: a pass whose staging is unreadable
/// restarts, a staging file with no pass is deleted. Returns the repaired
/// state.
fn recover(root: &Path, repo: &str) -> Option<RefreshState> {
    let staging = staging_path(root, repo);
    let mut state = read_state(&state_path(root, repo));
    let has_pass = state.as_ref().is_some_and(|s| s.pass.is_some());
    if has_pass && fleet::read(&staging).is_none() {
        log::warn!("eta fleet refresh: {repo}: staging snapshot unreadable; restarting its pass");
        if let Some(s) = state.as_mut() {
            s.pass = None;
        }
    }
    if !state.as_ref().is_some_and(|s| s.pass.is_some()) && staging.exists() {
        let _ = std::fs::remove_file(&staging);
    }
    state
}

/// The pass window `(L, S)`: the pass in progress's, else a new pass's.
#[must_use]
pub fn pass_window(
    state: &RefreshState,
    kind: PassKind,
    backfill_days: i64,
    now: DateTime<Utc>,
) -> (DateTime<Utc>, DateTime<Utc>) {
    if let Some(pass) = &state.pass {
        return (pass.listed_at, pass.since);
    }
    let since = match kind {
        PassKind::Backfill => now - Duration::days(backfill_days),
        PassKind::Refresh => {
            state.watermark.unwrap_or(now) - Duration::seconds(WATERMARK_SLACK_SEC)
        }
    };
    (now, since)
}

/// Run one cycle over `targets`, forge only.
pub fn run_cycle(
    root: &Path,
    targets: &[RepoTarget],
    forge: &mut dyn ForgeRead,
    budgets: Budgets,
    now: DateTime<Utc>,
) -> CycleReport {
    run_cycle_with(root, targets, forge, None, budgets, now)
}

/// Run one cycle over `targets`; with `signoz`, SigNoz first and the forge
/// only as gap-fill (#10520).
pub fn run_cycle_with(
    root: &Path,
    targets: &[RepoTarget],
    forge: &mut dyn ForgeRead,
    mut signoz: Option<(&mut dyn SignozRead, Limits)>,
    budgets: Budgets,
    now: DateTime<Utc>,
) -> CycleReport {
    struct Planned<'a> {
        target: &'a RepoTarget,
        state: Option<RefreshState>,
        published: Option<FleetSnapshot>,
        kind: PassKind,
        resuming: bool,
    }
    let mut planned: Vec<Planned> = targets
        .iter()
        .map(|target| {
            let state = recover(root, &target.repo);
            let published = fleet::read(&fleet::snapshot_path(root, &target.repo));
            let (kind, resuming) = match state.as_ref().and_then(|s| s.pass.as_ref()) {
                Some(pass) => (pass.kind, true),
                None => (
                    choose_pass(state.as_ref(), published.is_some(), now, budgets.backfill_days),
                    false,
                ),
            };
            Planned {
                target,
                state,
                published,
                kind,
                resuming,
            }
        })
        .collect();
    // Refreshes first (the fit's existing inputs stay fresh), then backfills:
    // in-progress ones before new ones, each group by slug.
    let order = |p: &Planned| {
        (p.kind == PassKind::Backfill, !p.resuming, p.target.repo.to_ascii_lowercase())
    };
    planned.sort_by_key(order);

    let mut report = CycleReport {
        remaining: (budgets.refresh, budgets.backfill),
        ..CycleReport::default()
    };
    let mut halted: Option<StopReason> = None;
    let mut reserve_installs: BTreeSet<Installation> = BTreeSet::new();
    for p in planned {
        let repo = &p.target.repo;
        // A skipped repo still reports the snapshot it is left with, so a
        // stale one is visible on its record.
        let skipped = |pass: Option<PassKind>, stop: StopReason| {
            let mut r = RepoReport::skipped(repo, pass, stop);
            r.snapshot_id = p.published.as_ref().map(|s| s.snapshot_id.clone());
            r.as_of = p.published.as_ref().map(|s| s.as_of);
            r
        };
        let reader = match &p.target.reader {
            Ok(reader) => reader,
            Err(NoReader::NoReader) => {
                report.repos.push(skipped(None, StopReason::NoReader));
                continue;
            }
            Err(NoReader::UnsupportedForge) => {
                report
                    .repos
                    .push(skipped(None, StopReason::UnsupportedForge));
                continue;
            }
        };
        let skip = halted.or_else(|| {
            reserve_installs
                .contains(&reader.installation(repo))
                .then_some(StopReason::Reserve)
        });
        let remaining = match p.kind {
            PassKind::Refresh => &mut report.remaining.0,
            PassKind::Backfill => &mut report.remaining.1,
        };
        let skip = skip.or_else(|| (*remaining == 0).then_some(StopReason::Budget));
        if let Some(stop) = skip {
            if p.kind == PassKind::Backfill && !p.resuming {
                let pended =
                    pend_backfill(root, repo, p.state.clone(), now, budgets.backfill_days, stop);
                if let Err(e) = pended {
                    log::warn!("eta fleet refresh: {repo}: could not record its due backfill: {e}");
                }
            }
            let mut r = skipped(Some(p.kind), stop);
            r.reader_app = Some(reader.app_id.clone());
            r.pass_done = p
                .state
                .as_ref()
                .and_then(|s| s.pass.as_ref())
                .map_or(0, |pass| pass.done.len() as u64);
            report.repos.push(r);
            continue;
        }
        let before = *remaining;
        let started = std::time::Instant::now();
        let mut walk = Walk {
            root,
            target: p.target,
            reader,
            forge: &mut *forge,
            remaining,
            reserve: budgets.reserve,
            below_reserve: false,
            report: RepoReport::skipped(repo, Some(p.kind), StopReason::Complete),
            reset_epoch: None,
            gap_left: None,
        };
        walk.report.reader_app = Some(reader.app_id.clone());
        let state = p.state.unwrap_or_else(|| RefreshState::new(repo));
        let load = signoz.as_mut().map(|(reader, limits)| {
            let window = pass_window(&state, p.kind, budgets.backfill_days, now);
            signoz_history::load(repo, &mut **reader, *limits, window, budgets.backfill_days)
        });
        let plan = walk.apply(load, budgets.gap_fill);
        let stop = walk.run(state, p.published, p.kind, budgets.backfill_days, now, plan);
        let spent = before - *walk.remaining;
        let below_reserve = walk.below_reserve;
        let reset = walk.reset_epoch;
        let mut r = walk.report;
        r.stop = stop;
        r.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        signoz_history::note_report(root, &r, now);
        match p.kind {
            PassKind::Refresh => report.refresh_calls += spent,
            PassKind::Backfill => report.backfill_calls += spent,
        }
        match stop {
            StopReason::RateLimited => {
                report.rate_limited = Some(reset);
                halted = Some(StopReason::RateLimited);
            }
            StopReason::BreakerOpen | StopReason::Shutdown => halted = Some(stop),
            _ => {}
        }
        if below_reserve || stop == StopReason::Reserve {
            reserve_installs.insert(reader.installation(repo));
        }
        report.repos.push(r);
    }
    report.backfill_in_progress_since = backfill_since(root, targets);
    report
}

/// Record a due backfill that a cycle skips before its first call (`reserve`,
/// `budget`, a halt, a gated cycle) as in progress from `now` (#10292), so
/// [`backfill_since`] sees it and the fit is held — for [`FIT_HOLD_HOURS`]
/// from this first skip, since a later cycle resumes the pass and never
/// rewrites its `L`. The on-disk shape is the one a first-call stop inside a
/// walk already checkpoints: empty staging **first** (without it [`recover`]
/// would drop the pass and the next skip would re-pend at a new `L`), then the
/// state. A no-op when a pass is already in progress.
///
/// # Errors
///
/// The staging or state write failed; the repo is then not held (fail open).
pub fn pend_backfill(
    root: &Path,
    repo: &str,
    state: Option<RefreshState>,
    now: DateTime<Utc>,
    backfill_days: i64,
    stop: StopReason,
) -> std::io::Result<()> {
    let mut state = state.unwrap_or_else(|| RefreshState::new(repo));
    if state.pass.is_some() {
        return Ok(());
    }
    fleet::write(&staging_path(root, repo), &FleetSnapshot::empty(repo))?;
    state.pass = Some(Pass {
        kind: PassKind::Backfill,
        listed_at: now,
        since: now - Duration::days(backfill_days),
        next_page: 1,
        done: Vec::new(),
        head_etag: None,
        partial: None,
    });
    state.last_stop = Some(LastStop {
        at: now,
        reason: stop,
    });
    write_state(&state_path(root, repo), &state)
}

/// [`pend_backfill`] for every target a gated cycle (`backoff`,
/// `breaker_open`) skips whole: each one with a reader, no pass in progress,
/// and a backfill due. Targets with no usable reader are never pended — they
/// are never served, so they would only hold the fit.
pub fn pend_due_backfills(
    root: &Path,
    targets: &[RepoTarget],
    now: DateTime<Utc>,
    backfill_days: i64,
    stop: StopReason,
) {
    for target in targets.iter().filter(|t| t.reader.is_ok()) {
        let repo = &target.repo;
        let state = recover(root, repo);
        if state.as_ref().is_some_and(|s| s.pass.is_some()) {
            continue;
        }
        let published = fleet::read(&fleet::snapshot_path(root, repo)).is_some();
        if choose_pass(state.as_ref(), published, now, backfill_days) != PassKind::Backfill {
            continue;
        }
        if let Err(e) = pend_backfill(root, repo, state, now, backfill_days, stop) {
            log::warn!("eta fleet refresh: {repo}: could not record its due backfill: {e}");
        }
    }
}

/// The earliest listing instant `L` of any backfill pass in progress among
/// `targets` — what [`fit_held`] reads.
#[must_use]
pub fn backfill_since(root: &Path, targets: &[RepoTarget]) -> Option<DateTime<Utc>> {
    targets
        .iter()
        .filter_map(|t| read_state(&state_path(root, &t.repo)))
        .filter_map(|s| s.pass)
        .filter(|pass| pass.kind == PassKind::Backfill)
        .map(|pass| pass.listed_at)
        .min()
}

/// One successful answer.
pub(super) struct Answer {
    status: u16,
    etag: Option<String>,
    pub(super) body: String,
}

/// One repo's walk within a cycle.
pub(super) struct Walk<'a> {
    root: &'a Path,
    pub(super) target: &'a RepoTarget,
    reader: &'a Reader,
    forge: &'a mut dyn ForgeRead,
    remaining: &'a mut u64,
    reserve: u64,
    below_reserve: bool,
    pub(super) report: RepoReport,
    reset_epoch: Option<i64>,
    /// Gap-fill calls left this cycle; `None` when unbounded (#10520).
    pub(super) gap_left: Option<u64>,
}

impl Walk<'_> {
    /// One budgeted, breaker-checked call.
    pub(super) fn call(
        &mut self,
        url: &str,
        etag: Option<&str>,
        op: ForgeOp,
    ) -> Result<Answer, StopReason> {
        if self.forge.shutting_down() {
            return Err(StopReason::Shutdown);
        }
        if self.forge.breaker_open() {
            return Err(StopReason::BreakerOpen);
        }
        if self.below_reserve {
            return Err(StopReason::Reserve);
        }
        if *self.remaining == 0 {
            return Err(StopReason::Budget);
        }
        self.charge_gap_fill()?;
        *self.remaining -= 1;
        self.report.forge_calls += 1;
        let read = self.forge.get(self.target, self.reader, url, etag, op);
        let seen = match &read {
            Read::Ok { remaining, .. } | Read::Failed { remaining, .. } => *remaining,
        };
        if let Some(left) = seen {
            self.report.ratelimit_remaining_min = Some(
                self.report
                    .ratelimit_remaining_min
                    .map_or(left, |m| m.min(left)),
            );
            // The page already paid for is kept; the floor stops the next call.
            self.below_reserve |= left < self.reserve;
        }
        match read {
            Read::Ok {
                status, etag, body, ..
            } => {
                if status == 304 {
                    self.report.not_modified_calls += 1;
                }
                Ok(Answer { status, etag, body })
            }
            Read::Failed {
                failure,
                reset_epoch,
                detail,
                ..
            } => {
                log::info!(
                    "eta fleet refresh: {}: read failed ({failure:?}): {detail}",
                    self.target.repo
                );
                Err(match failure {
                    ReadFailure::RateLimited => {
                        self.reset_epoch = reset_epoch;
                        StopReason::RateLimited
                    }
                    ReadFailure::Coverage => StopReason::Coverage,
                    ReadFailure::Other => StopReason::ForgeError,
                })
            }
        }
    }

    /// Run the repo's pass for this cycle; returns why it stopped. With a
    /// covered SigNoz `plan` no listing call is made (#10520).
    fn run(
        &mut self,
        mut state: RefreshState,
        published: Option<FleetSnapshot>,
        kind: PassKind,
        backfill_days: i64,
        now: DateTime<Utc>,
        plan: Option<Plan>,
    ) -> StopReason {
        let repo = self.target.repo.clone();
        let published_path = fleet::snapshot_path(self.root, &repo);
        let staging_file = staging_path(self.root, &repo);
        let state_file = state_path(self.root, &repo);
        let before = published.as_ref().map_or(0, |s| s.samples.len());
        let mut prefetched: Option<Answer> = None;
        let (listed_at, since) = pass_window(&state, kind, backfill_days, now);

        let (mut pass, mut staging) = match state.pass.take() {
            Some(pass) => match fleet::read(&staging_file) {
                Some(staging) => (pass, staging),
                None => return self.finish(StopReason::WriteError, &published_path, before),
            },
            None => {
                let staging = match (kind, &published) {
                    (PassKind::Refresh, Some(p)) => p.clone(),
                    _ => FleetSnapshot::empty(&repo),
                };
                if kind == PassKind::Refresh && plan.is_none() {
                    let etag = state.listing_etag.clone();
                    match self.call(&listing_url(&repo, 1), etag.as_deref(), ISSUE_LIST) {
                        Ok(answer) if answer.status == 304 => {
                            let mut snapshot = staging;
                            snapshot.merge(&[], listed_at);
                            if fleet::write(&published_path, &snapshot).is_err() {
                                return self.finish(
                                    StopReason::WriteError,
                                    &published_path,
                                    before,
                                );
                            }
                            state.watermark = Some(listed_at);
                            state.last_stop = Some(LastStop {
                                at: now,
                                reason: StopReason::NotModified,
                            });
                            if write_state(&state_file, &state).is_err() {
                                return self.finish(
                                    StopReason::WriteError,
                                    &published_path,
                                    before,
                                );
                            }
                            return self.finish(StopReason::NotModified, &published_path, before);
                        }
                        Ok(answer) => prefetched = Some(answer),
                        Err(stop) => {
                            self.record_stop(&state_file, &mut state, stop, now);
                            return self.finish(stop, &published_path, before);
                        }
                    }
                }
                let pass = Pass {
                    kind,
                    listed_at,
                    since,
                    next_page: 1,
                    done: Vec::new(),
                    head_etag: None,
                    partial: None,
                };
                (pass, staging)
            }
        };

        let stop = match &plan {
            Some(plan) => self.walk_signoz(&mut pass, &mut staging, plan),
            None => self.walk(&state, &mut pass, &mut staging, prefetched),
        };
        self.report.pass_done = pass.done.len() as u64;
        if stop != StopReason::Complete {
            // Checkpoint and leave the published snapshot untouched.
            state.pass = Some(pass);
            let checkpoint =
                self.checkpoint(&staging_file, &staging, &state_file, &mut state, stop, now);
            let stop = if checkpoint {
                stop
            } else {
                StopReason::WriteError
            };
            return self.finish(stop, &published_path, before);
        }
        // Promote: the snapshot is as of `L` even when nothing was read.
        staging.merge(&[], pass.listed_at);
        if fleet::write(&published_path, &staging).is_err() {
            state.pass = Some(pass);
            let _ = self.checkpoint(
                &staging_file,
                &staging,
                &state_file,
                &mut state,
                StopReason::WriteError,
                now,
            );
            return self.finish(StopReason::WriteError, &published_path, before);
        }
        state.watermark = Some(pass.listed_at);
        state.listing_etag = pass.head_etag.clone();
        if pass.kind == PassKind::Backfill {
            state.covered_since = Some(pass.since);
            state.derivation_rev = DERIVATION_REV;
        }
        state.pass = None;
        state.last_stop = Some(LastStop {
            at: now,
            reason: StopReason::Complete,
        });
        if write_state(&state_file, &state).is_err() {
            return self.finish(StopReason::WriteError, &published_path, before);
        }
        let _ = std::fs::remove_file(&staging_file);
        self.report.promoted = true;
        self.finish(StopReason::Complete, &published_path, before)
    }

    /// Walk listing pages from `pass.next_page` until the pass completes or
    /// stops, checkpointing after every full page.
    fn walk(
        &mut self,
        state: &RefreshState,
        pass: &mut Pass,
        staging: &mut FleetSnapshot,
        mut prefetched: Option<Answer>,
    ) -> StopReason {
        let repo = self.target.repo.clone();
        let staging_file = staging_path(self.root, &repo);
        let state_file = state_path(self.root, &repo);
        loop {
            let answer = match prefetched.take() {
                Some(answer) => answer,
                None => match self.call(&listing_url(&repo, pass.next_page), None, ISSUE_LIST) {
                    Ok(answer) => answer,
                    Err(stop) => return stop,
                },
            };
            if pass.next_page == 1 {
                pass.head_etag = answer.etag.clone();
            }
            let Ok(rows) = parse_listing(&answer.body) else {
                log::info!(
                    "eta fleet refresh: {repo}: listing page {} unparseable",
                    pass.next_page
                );
                return StopReason::ForgeError;
            };
            let mut reached_end = rows.len() < PER_PAGE;
            for row in &rows {
                if row.updated_at < pass.since {
                    reached_end = true;
                    break;
                }
                let Some(pr) = &row.pr else { continue };
                if pass.done.binary_search(&row.number).is_ok() {
                    continue;
                }
                match self.timeline(pass, row.number) {
                    Ok(Some(events)) => {
                        staging.merge(&[history(row.number, pr, events, true)], pass.listed_at);
                    }
                    Ok(None) => self.report.timelines_incomplete += 1,
                    Err(stop) => return stop,
                }
                self.report.prs_read += 1;
                if let Err(at) = pass.done.binary_search(&row.number) {
                    pass.done.insert(at, row.number);
                }
            }
            if reached_end {
                return StopReason::Complete;
            }
            pass.next_page += 1;
            // Checkpoint after every full page: staging first, then state.
            let mut state = state.clone();
            state.pass = Some(pass.clone());
            if fleet::write(&staging_file, staging).is_err()
                || write_state(&state_file, &state).is_err()
            {
                return StopReason::WriteError;
            }
        }
    }

    /// Persist a stop with no pass in progress (only `last_stop` changes).
    fn record_stop(
        &self,
        state_file: &Path,
        state: &mut RefreshState,
        stop: StopReason,
        now: DateTime<Utc>,
    ) {
        state.last_stop = Some(LastStop {
            at: now,
            reason: stop,
        });
        let _ = write_state(state_file, state);
    }

    /// Staging first, then the state (with `last_stop`). `false` on a write
    /// failure.
    fn checkpoint(
        &self,
        staging_file: &Path,
        staging: &FleetSnapshot,
        state_file: &Path,
        state: &mut RefreshState,
        stop: StopReason,
        now: DateTime<Utc>,
    ) -> bool {
        state.last_stop = Some(LastStop {
            at: now,
            reason: stop,
        });
        fleet::write(staging_file, staging).is_ok() && write_state(state_file, state).is_ok()
    }

    /// Fill in the published-snapshot fields and return `stop`.
    fn finish(&mut self, stop: StopReason, published_path: &Path, before: usize) -> StopReason {
        if let Some(after) = fleet::read(published_path) {
            self.report.samples_added = after.samples.len() as i64 - before as i64;
            self.report.snapshot_id = Some(after.snapshot_id);
            self.report.as_of = Some(after.as_of);
        }
        stop
    }
}
