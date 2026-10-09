//! Per-issue dispatch disposition spans (Issue #9222): one
//! `loom.dispatch.disposition` span per ready-queue row when its disposition
//! changes, on a periodic refresh, or once when the row leaves the queue — so
//! SigNoz can answer "why hasn't `owner/repo#N` started" for any queued issue,
//! not just the ones that reached a `dispatch()` attempt
//! ([`super::dispatch`]'s `loom.dispatch.admission`). Each span also carries
//! the row's queue position metadata (Issue #9669): `loom.queue.candidate_rank`
//! (1-indexed, plan position when the row has one, else its comparator rank),
//! `loom.queue.total_candidates` (the tick's ready-queue depth) and
//! `loom.queue.priority_score` (the comparator keys that placed it, as one
//! compact JSON object).
//!
//! # Where this runs, and why not the tick loop
//!
//! Driven from the collector's periodic sample
//! (`observability::collector::sample_snapshots`, the same cadence as
//! [`super::super::queue_snapshot::record`] / [`super::stage_dwell::record`]),
//! reading [`crate::work_finder::last_tick_summary`] — never from the
//! work-finder tick itself, which must never block on a `gh repo view` probe
//! (see `queue_snapshot`'s module doc for the identical rationale). A
//! disposition can therefore be missed if it lasts less than one collector
//! interval; only the last-tick state per interval is exported. That is an
//! accepted tradeoff for "why is it waiting right now", not "a complete
//! history".
//!
//! [`DispositionTracker`] is the pure core — modeled on
//! [`super::dwell::DwellTracker`] — and [`record`] is the only side effect; it
//! returns before doing any work when no ops sink is registered.
//!
//! # Cadence
//!
//! One `(repo, issue)` row is exported when:
//! - **transition**: its disposition differs from the last one this process
//!   emitted for that key, including first sight; or
//! - **refresh**: the last emission for that key is older than
//!   [`REFRESH_SECS_ENV`] (default [`DEFAULT_REFRESH_SECS`], ten minutes).
//!
//! A terminal `left_queue` span is emitted once when a previously emitted key
//! disappears from a repo whose listing succeeded this tick. A key belonging
//! to a repo whose listing **failed** is left untouched — neither emitted nor
//! dropped — exactly like [`super::dwell::DwellTracker`]'s treatment of an
//! incomplete repo, so a transient `gh` failure never reads as "the issue
//! left the queue".
//!
//! # Cardinality bound
//!
//! At most [`MAX_ROWS`] rows are turned into spans per [`record`] call;
//! further rows are dropped and counted on
//! [`MetricName::QueueDispositionRowsDropped`] with `reason = "truncated"`, as
//! is a row whose repo root never resolved to a forge slug (`reason =
//! "unresolved"`). A single-workspace-loop tick names every row `workspace
//! #N` (never an absolute path), which never resolves either — those rows are
//! dropped the same way, exactly like `queue_snapshot`.
//!
//! # Single-workspace loop
//!
//! The single-workspace work-finder loop records no per-issue queue rows at
//! all (`WorkFinderTickSummary::queue` is empty), so it never has anything to
//! export here — the same limitation `queue_snapshot` and `stage_dwell`
//! already document.
//!
//! # Repo lockout attributes (Issue #9674)
//!
//! Rows the #4123 open-PR guard refused (`pr-open-skip`,
//! [`QueueDisposition::OpenPr`]) are the observable signature of a repository
//! whose backlog is frozen behind an open linked PR. Each `open_pr` span
//! carries its repo's lockout weight — `lockout.frozen_candidates_count`,
//! `lockout.frozen_points_sum`, `lockout.duration_seconds` (an in-process
//! observation floor; see [`super::lockout`]) — so SigNoz can rank which
//! repo's lockout is starving the most ready work. The computation is pure
//! over this tick's rows; nothing here ever reads the forge.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Duration, Utc};

use crate::hard_exclusion::HARD_EXCLUSION_LABELS;
use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::telemetry::queue_snapshot::QueueRepoRef;
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};
use crate::telemetry::RepoVisibility;
use crate::types::{PlanKey, QueueDisposition, WorkFinderTickSummary};
use crate::work_finder::halt_cause::HaltCause;
use crate::work_finder::{PARK_LABELS, SKIP_LABELS};

/// Env override for the refresh cadence, in seconds.
pub const REFRESH_SECS_ENV: &str = "LOOM_DISPATCH_DISPOSITION_REFRESH_SECS";
/// Default refresh cadence: ten minutes.
pub const DEFAULT_REFRESH_SECS: i64 = 600;

/// Most rows turned into spans per [`record`] call. A row past this is
/// dropped and counted with `reason = "truncated"`.
pub const MAX_ROWS: usize = 256;

/// How a row's span relates to the previous emission this process made for
/// the same `(repo, issue)` key. The wire value of
/// `loom.queue.transition`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// First sight of this key, or its disposition changed since last emitted.
    Changed,
    /// Same disposition as last emitted, re-sent because the refresh window
    /// elapsed.
    Refresh,
    /// A previously emitted key is no longer in a successfully listed repo's
    /// ready queue.
    LeftQueue,
}

impl Transition {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Changed => "changed",
            Self::Refresh => "refresh",
            Self::LeftQueue => "left_queue",
        }
    }
}

/// One ready-queue row, reduced to what [`DispositionTracker`] and the span
/// builder need. Built by [`build_rows`] from a [`WorkFinderTickSummary`] plus
/// its resolved repos.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispositionRow {
    /// Forge `owner/repo`. Never a local path (Issue #9222).
    pub slug: String,
    pub visibility: RepoVisibility,
    pub issue: u32,
    pub rank: usize,
    pub disposition: QueueDisposition,
    /// Present only for `Parked`/`HardExclusion` rows whose label is in the
    /// closed `PARK_LABELS ∪ SKIP_LABELS` vocabulary — see
    /// [`allowed_park_label`].
    pub park_label: Option<String>,
    /// Present only for `OpenPr` rows — see [`open_pr_number`].
    pub pr_number: Option<u32>,
    /// 1-indexed queue position at this tick (Issue #9669): the row's
    /// dispatch-plan `position` in the shaped pass-2 candidate order when the
    /// plan annotated it, else its bare comparator `rank` — a blocked row's
    /// would-be position once unblocked. Always present on a queue row.
    pub candidate_rank: u32,
    /// The tick's ready-queue row count — the denominator of
    /// [`Self::candidate_rank`] (Issue #9669).
    pub total_candidates: usize,
    /// The comparator keys that placed the row, as one compact JSON object in
    /// comparator order — see [`priority_score`]. `None` when the row carries
    /// no plan annotation (no keys) or the JSON exceeds the span-attribute
    /// byte bound.
    pub priority_score: Option<String>,
    /// The issue's resolved story-point size (#9432, Issue #9674) — `None`
    /// when unsized or a labeled defect (never a guess).
    pub story_points: Option<u32>,
    /// Present only for `WorkspaceHalted` rows whose detail is the closed
    /// `work_finder::halt_cause` vocabulary (#9017) — see [`halt_cause_token`]
    /// (#9673).
    pub halt_cause: Option<String>,
}

/// One row's outcome for a [`record`] sample: enough to build its span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Emission {
    pub slug: String,
    pub visibility: RepoVisibility,
    pub issue: u32,
    /// Absent for [`Transition::LeftQueue`]: the row is no longer ranked.
    pub rank: Option<usize>,
    pub disposition: QueueDisposition,
    /// Present only for [`Transition::Changed`] after the first sighting.
    pub previous_disposition: Option<QueueDisposition>,
    pub transition: Transition,
    pub park_label: Option<String>,
    pub pr_number: Option<u32>,
    /// 1-indexed queue position at this tick (Issue #9669); absent on a
    /// `left_queue` emission, whose row is no longer ranked.
    pub candidate_rank: Option<u32>,
    /// The tick's ready-queue row count (Issue #9669); absent on a
    /// `left_queue` emission.
    pub total_candidates: Option<usize>,
    /// The comparator keys that placed the row, as one compact JSON object
    /// (Issue #9669); absent without a plan annotation or on a `left_queue`
    /// emission.
    pub priority_score: Option<String>,
    /// Present only for a `WorkspaceHalted` row whose detail named a
    /// closed-vocabulary halt cause — see [`halt_cause_token`] (#9673).
    pub halt_cause: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TrackedState {
    disposition: QueueDisposition,
    visibility: RepoVisibility,
    emitted_at: DateTime<Utc>,
}

/// Per-`(slug, issue)` last-emitted disposition and instant (Issue #9222).
/// Modeled on [`super::dwell::DwellTracker`]: a plain struct with a pure
/// [`Self::diff`], driven by a process-global instance only at the [`record`]
/// seam.
#[derive(Debug, Default)]
pub struct DispositionTracker {
    last: HashMap<(String, u32), TrackedState>,
}

impl DispositionTracker {
    /// Fold one sample's rows into the tracker, returning every row that must
    /// be exported this sample: a transition, a refresh, or a row that left a
    /// successfully-listed repo's queue since the last sample.
    ///
    /// `failed_slugs` are the repos whose ready-issue listing failed this
    /// tick, by forge slug — the same repo/tick distinction
    /// [`super::dwell::DwellTracker::observe`]'s `incomplete_repos` makes,
    /// applied here to slugs instead of repo roots. A key belonging to a repo
    /// **not** in `failed_slugs` is treated as successfully listed even if it
    /// contributed no rows this tick (an empty ready queue reads the same as
    /// "not managed this tick" — both mean "not evidence of failure", exactly
    /// like `DwellTracker`'s convention).
    pub fn diff(
        &mut self,
        rows: &[DispositionRow],
        failed_slugs: &[String],
        now: DateTime<Utc>,
        refresh: Duration,
    ) -> Vec<Emission> {
        let mut next: HashMap<(String, u32), TrackedState> = HashMap::new();
        let mut emissions = Vec::new();

        for row in rows {
            let key = (row.slug.clone(), row.issue);
            let previous = self.last.get(&key).copied();
            let outcome = match previous {
                None => Some((Transition::Changed, None)),
                Some(state) if state.disposition != row.disposition => {
                    Some((Transition::Changed, Some(state.disposition)))
                }
                Some(state) if now.signed_duration_since(state.emitted_at) >= refresh => {
                    Some((Transition::Refresh, None))
                }
                Some(_) => None,
            };
            let emitted_at = match (&outcome, previous) {
                (Some(_), _) | (None, None) => now,
                (None, Some(state)) => state.emitted_at,
            };
            next.insert(
                key,
                TrackedState {
                    disposition: row.disposition,
                    visibility: row.visibility,
                    emitted_at,
                },
            );
            if let Some((transition, previous_disposition)) = outcome {
                emissions.push(Emission {
                    slug: row.slug.clone(),
                    visibility: row.visibility,
                    issue: row.issue,
                    rank: Some(row.rank),
                    disposition: row.disposition,
                    previous_disposition,
                    transition,
                    park_label: row.park_label.clone(),
                    pr_number: row.pr_number,
                    candidate_rank: Some(row.candidate_rank),
                    total_candidates: Some(row.total_candidates),
                    priority_score: row.priority_score.clone(),
                    halt_cause: row.halt_cause.clone(),
                });
            }
        }

        let seen: HashSet<(String, u32)> = rows.iter().map(|r| (r.slug.clone(), r.issue)).collect();
        for (key, state) in &self.last {
            if seen.contains(key) {
                continue; // handled in the loop above
            }
            if failed_slugs.iter().any(|slug| slug == &key.0) {
                // This repo's listing failed: keep the state, no emission.
                next.insert(key.clone(), *state);
            } else {
                emissions.push(Emission {
                    slug: key.0.clone(),
                    visibility: state.visibility,
                    issue: key.1,
                    rank: None,
                    disposition: state.disposition,
                    previous_disposition: None,
                    transition: Transition::LeftQueue,
                    park_label: None,
                    pr_number: None,
                    candidate_rank: None,
                    total_candidates: None,
                    priority_score: None,
                    halt_cause: None,
                });
                // Dropped: not reinserted into `next`, so this key reads as
                // "first sight" (`Changed`) if the issue ever reappears.
            }
        }

        self.last = next;
        emissions
    }
}

/// Dropped-row counts from [`build_rows`], for the
/// [`MetricName::QueueDispositionRowsDropped`] metric.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DroppedCounts {
    pub unresolved: usize,
    pub truncated: usize,
}

/// The label on `detail`, only for rows whose disposition carries a label in
/// a closed vocabulary — never a repo-configured extra skip label or other
/// free-form text: `Parked` rows export `PARK_LABELS ∪ SKIP_LABELS` entries;
/// `HardExclusion` rows export `hard_exclusion::HARD_EXCLUSION_LABELS`
/// entries (e.g. `external`), so a span names which rule declined the issue
/// (#9672). Anything else — including a hard-exclusion label on a
/// non-exclusion row — exports nothing.
#[must_use]
fn allowed_park_label(disposition: QueueDisposition, detail: Option<&str>) -> Option<String> {
    let label = detail?;
    let in_vocabulary = match disposition {
        QueueDisposition::Parked => PARK_LABELS.contains(&label) || SKIP_LABELS.contains(&label),
        QueueDisposition::HardExclusion => HARD_EXCLUSION_LABELS.contains(&label),
        _ => false,
    };
    in_vocabulary.then(|| label.to_string())
}

/// The PR number in an `OpenPr` row's detail (`"open PR #<n>"`, the text
/// `work_finder::tick_report::classify` formats) — structured extraction,
/// never the free-form text itself. Mirrors
/// `telemetry::queue_snapshot::exportable_detail`'s allowlist-by-disposition
/// rule.
#[must_use]
fn open_pr_number(disposition: QueueDisposition, detail: Option<&str>) -> Option<u32> {
    if disposition != QueueDisposition::OpenPr {
        return None;
    }
    let (_, digits) = detail?.rsplit_once('#')?;
    digits.parse().ok()
}

/// A row's composite priority weight (Issue #9669): its comparator keys as
/// one compact JSON object in comparator order, e.g.
/// `{"operator_priority":false,"workspace_priority":100,…}`. The dispatch
/// comparator is lexicographic over named keys, so there is no numeric score
/// to fabricate — the key tuple *is* the weight that placed the row, taken
/// from the row's plan annotation (the same projection the dispatch plan
/// publishes, Issue #9288). `None` without keys (no plan annotation), or when
/// the JSON would exceed the 256-byte span-attribute bound, which would drop
/// it at emission anyway.
#[must_use]
fn priority_score(keys: &[PlanKey]) -> Option<String> {
    if keys.is_empty() {
        return None;
    }
    let score = format!(
        "{{{}}}",
        keys.iter()
            .map(|key| format!("\"{}\":{}", key.name, key.value))
            .collect::<Vec<_>>()
            .join(",")
    );
    (score.len() <= 256).then_some(score)
}

/// The closed-vocabulary halt cause in a `WorkspaceHalted` row's `detail`
/// (#9017) — validated through [`HaltCause::from_wire`] and re-emitted as its
/// canonical `as_str` token, never the raw text (#9673). A cause-less legacy
/// row (a pre-#9017 caller) and any token outside the vocabulary export
/// nothing.
#[must_use]
fn halt_cause_token(disposition: QueueDisposition, detail: Option<&str>) -> Option<String> {
    if disposition != QueueDisposition::WorkspaceHalted {
        return None;
    }
    HaltCause::from_wire(detail?).map(|cause| cause.as_str().to_string())
}

/// Build this tick's disposition rows from `summary`'s ranked queue, resolving
/// each row's repo through `repos` (as [`super::super::repo_ref::resolve_repo_refs`]
/// already resolved it). A row whose repo did not resolve is dropped and
/// counted as `unresolved`; a row past [`MAX_ROWS`] is dropped and counted as
/// `truncated`. Pure.
#[must_use]
pub fn build_rows(
    summary: &WorkFinderTickSummary,
    repos: &HashMap<String, QueueRepoRef>,
) -> (Vec<DispositionRow>, DroppedCounts) {
    let total_candidates = summary.queue.len();
    let mut rows = Vec::new();
    let mut dropped = DroppedCounts::default();
    for row in &summary.queue {
        let Some(repo) = repos.get(&row.repo) else {
            dropped.unresolved += 1;
            continue;
        };
        if rows.len() >= MAX_ROWS {
            dropped.truncated += 1;
            continue;
        }
        rows.push(DispositionRow {
            slug: repo.repo.clone(),
            visibility: repo.visibility,
            issue: row.issue,
            rank: row.rank,
            disposition: row.disposition,
            park_label: allowed_park_label(row.disposition, row.detail.as_deref()),
            pr_number: open_pr_number(row.disposition, row.detail.as_deref()),
            // The plan's pass-2 position is the rank among *eligible*
            // candidates; the bare comparator rank is the fallback for a row
            // the plan gave no position (blocked, or a pre-#9288 payload) —
            // where it would sit once unblocked (Issue #9669).
            candidate_rank: row
                .plan
                .position
                .unwrap_or_else(|| u32::try_from(row.rank).unwrap_or(u32::MAX)),
            total_candidates,
            priority_score: priority_score(&row.plan.keys),
            story_points: row.story_points,
            halt_cause: halt_cause_token(row.disposition, row.detail.as_deref()),
        });
    }
    (rows, dropped)
}

/// The forge slugs of `summary`'s repos whose listing failed this tick, from
/// the same `repos` map [`build_rows`] used. A failed repo whose root never
/// resolved to a slug cannot be named here — its previously tracked keys (if
/// any) fall back to [`DispositionTracker::diff`]'s default "successfully
/// listed" treatment, the same edge case `queue_snapshot` accepts for its own
/// `listing_failed_unresolved` counter.
#[must_use]
fn failed_slugs(
    summary: &WorkFinderTickSummary,
    repos: &HashMap<String, QueueRepoRef>,
) -> Vec<String> {
    // #11139: a partial listing's missing rows are as unknown as a failed one's.
    summary
        .listing_not_whole()
        .filter_map(|root| repos.get(root).map(|r| r.repo.clone()))
        .collect()
}

/// The refresh threshold: `raw` seconds when it is a positive integer,
/// otherwise [`DEFAULT_REFRESH_SECS`].
#[must_use]
pub fn refresh_secs(raw: Option<&str>) -> i64 {
    raw.and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_REFRESH_SECS)
}

fn visibility_str(visibility: RepoVisibility) -> &'static str {
    match visibility {
        RepoVisibility::Public => "public",
        RepoVisibility::Private => "private",
    }
}

fn int(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// The metric points for `dropped`, one per non-zero reason.
#[must_use]
pub fn dropped_points(dropped: DroppedCounts) -> Vec<MetricPoint> {
    let mut points = Vec::new();
    if dropped.unresolved > 0 {
        points.push(
            MetricPoint::int(MetricName::QueueDispositionRowsDropped, int(dropped.unresolved))
                .label("reason", "unresolved"),
        );
    }
    if dropped.truncated > 0 {
        points.push(
            MetricPoint::int(MetricName::QueueDispositionRowsDropped, int(dropped.truncated))
                .label("reason", "truncated"),
        );
    }
    points
}

/// One `loom.dispatch.disposition` span for `emission`, sampled at `now`.
/// Parented to `parent` (the tick's own span context) when its tick matches
/// this sample; otherwise a deterministic root of its own, exactly like
/// [`super::dispatch::tick_span`]'s own root when no parent applies. Span IDs
/// are always derived from `(slug, issue, now)`, never random — the
/// trace-identity policy every span in this package follows.
///
/// An `OpenPr` emission (a `pr-open-skip` row) in a repo this sample says is
/// locked also carries the repo's lockout attributes (Issue #9674):
/// `lockout.duration_seconds` from the global tracker,
/// `lockout.frozen_candidates_count` and `lockout.frozen_points_sum` from
/// `lockouts`. `LeftQueue` emissions never carry them — the row has already
/// left the queue, so its repo's current lock state is not this row's story.
#[must_use]
pub fn build_span(
    emission: &Emission,
    now: DateTime<Utc>,
    parent: Option<&TraceContext>,
    lockouts: &HashMap<String, super::lockout::RepoLockout>,
) -> SpanRecord {
    let slug: &str = &emission.slug;
    let issue = emission.issue.to_string();
    let issue_str: &str = &issue;
    let instant = crate::telemetry::trace::instant(now);
    let instant_str: &str = &instant;
    let (context, parent_span_id) = match parent {
        Some(parent) => (
            parent.derived_child(&[
                SpanName::DispatchDisposition.as_str(),
                slug,
                issue_str,
                instant_str,
            ]),
            Some(parent.span_id.clone()),
        ),
        None => (
            TraceContext::derived(
                SpanName::DispatchDisposition.as_str(),
                &[slug, issue_str, instant_str],
            ),
            None,
        ),
    };
    let mut attributes: TraceAttributes = [
        ("loom.repo", emission.slug.clone()),
        ("loom.repo.visibility", visibility_str(emission.visibility).to_string()),
        ("loom.issue", issue),
        ("loom.queue.disposition", emission.disposition.as_str().to_string()),
        ("loom.queue.state", emission.disposition.state().to_string()),
        ("loom.queue.transition", emission.transition.as_str().to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    if let Some(rank) = emission.rank {
        attributes.insert("loom.queue.rank".to_string(), rank.to_string());
    }
    if let Some(candidate_rank) = emission.candidate_rank {
        attributes.insert("loom.queue.candidate_rank".to_string(), candidate_rank.to_string());
    }
    if let Some(total) = emission.total_candidates {
        attributes.insert("loom.queue.total_candidates".to_string(), int(total).to_string());
    }
    if let Some(score) = &emission.priority_score {
        attributes.insert("loom.queue.priority_score".to_string(), score.clone());
    }
    if let Some(previous) = emission.previous_disposition {
        attributes
            .insert("loom.queue.previous_disposition".to_string(), previous.as_str().to_string());
    }
    if let Some(label) = &emission.park_label {
        attributes.insert("loom.queue.park_label".to_string(), label.clone());
    }
    if let Some(pr) = emission.pr_number {
        attributes.insert("loom.pr_number".to_string(), pr.to_string());
    }
    // Issue #9674: a pr-open-skip row is the lock it reports. Stamp the
    // repo-level lockout attributes on exactly those rows, so a SigNoz query
    // over `loom.queue.disposition = "open_pr"` spans ranks every repo's
    // lockout by frozen backlog without double counting the repo's other
    // (non-blocked) rows. A `LeftQueue` emission is excluded: the row has
    // already left the queue, so the repo's current lock state is not this
    // row's story.
    if emission.transition != Transition::LeftQueue
        && emission.disposition == QueueDisposition::OpenPr
    {
        if let Some(lockout) = lockouts.get(slug) {
            attributes.insert(
                "lockout.frozen_candidates_count".to_string(),
                int(lockout.backlog.candidates).to_string(),
            );
            attributes.insert(
                "lockout.frozen_points_sum".to_string(),
                i64::try_from(lockout.backlog.points)
                    .unwrap_or(i64::MAX)
                    .to_string(),
            );
            if let Some(secs) = lockout.duration_secs {
                attributes.insert("lockout.duration_seconds".to_string(), secs.to_string());
            }
        }
    }
    if let Some(cause) = &emission.halt_cause {
        attributes.insert("loom.queue.halt_cause".to_string(), cause.clone());
    }
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context,
        parent_span_id,
        name: SpanName::DispatchDisposition,
        started_at: now,
        ended_at: now,
        status: SpanStatus::Ok,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

static TRACKER: OnceLock<Mutex<DispositionTracker>> = OnceLock::new();
static LAST_SAMPLED_TICK: Mutex<Option<DateTime<Utc>>> = Mutex::new(None);

/// Sample the last work-finder tick and export a `loom.dispatch.disposition`
/// span for every row [`DispositionTracker::diff`] says needs one. A no-op
/// (no forge reads, no tracker mutation) without the OTLP ops sink, and a
/// no-op when the work finder has not ticked since the previous sample.
pub(in crate::observability) async fn record(slug_cache: &mut HashMap<String, String>) {
    let Some(sink) = super::global_ops_sink() else {
        return;
    };
    let Some(summary) = crate::work_finder::last_tick_summary() else {
        return;
    };
    let previous_sample = *LAST_SAMPLED_TICK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !super::super::queue_snapshot::is_new_tick(summary.at, previous_sample) {
        return;
    }
    let roots: HashSet<&str> = summary
        .queue
        .iter()
        .map(|r| r.repo.as_str())
        .chain(summary.listing_not_whole().map(String::as_str))
        .collect();
    let repos = super::super::repo_ref::resolve_repo_refs(roots, slug_cache).await;
    let (rows, dropped) = build_rows(&summary, &repos);
    let failed = failed_slugs(&summary, &repos);
    let now = Utc::now();
    // Issue #9674: aggregate this tick's pr-open-skip rows per repo, advance
    // the global lockout clock (insert-if-absent, failed listings preserved),
    // and stamp the resulting weights on the repo's `open_pr` spans below.
    let lockouts = super::lockout::sample(
        rows.iter()
            .map(|r| (r.slug.as_str(), r.disposition, r.story_points)),
        &failed,
        now,
    );
    let refresh = Duration::seconds(refresh_secs(std::env::var(REFRESH_SECS_ENV).ok().as_deref()));
    let emissions = TRACKER
        .get_or_init(|| Mutex::new(DispositionTracker::default()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .diff(&rows, &failed, now, refresh);
    let parent = super::dispatch::last_tick_context()
        .filter(|(at, _)| *at == summary.at)
        .map(|(_, context)| context);
    for emission in &emissions {
        sink.emit_span(build_span(emission, now, parent.as_ref(), &lockouts));
    }
    sink.emit_metrics_since(dropped_points(dropped), previous_sample);
    *LAST_SAMPLED_TICK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(summary.at);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "disposition_tests.rs"]
mod tests;
