//! Repository lockout metrics (Issue #9674): when a repo's ready backlog is
//! frozen behind the #4123 open-PR guard (`pr-open-skip`), the disposition and
//! admission spans for that repo's blocked candidates carry how long the lock
//! has held and how much ready work it is freezing.
//!
//! # Definitions
//!
//! A repo is **locked** on a tick when at least one of its ready-queue rows
//! has the [`QueueDisposition::OpenPr`] disposition — the work finder offered
//! the issue to `dispatch()` and the open-PR guard refused it (`pr-open-skip`).
//! The lock's weight:
//!
//! - `lockout.frozen_candidates_count` — how many ready issues the guard is
//!   blocking in that repo (the `OpenPr` row count).
//! - `lockout.frozen_points_sum` — the sum of those issues' resolved story
//!   points (`points:*` labels, `crate::story_points`). Unsized issues
//!   contribute nothing: absent is never zero (#9432).
//! - `lockout.duration_seconds` — elapsed time since the daemon **first
//!   observed** this repo locked, measured in-process by [`LockoutTracker`].
//!
//! # The duration clock is observational, not forensic
//!
//! The daemon never probes the forge for a PR's `createdAt` — the disposition
//! exporter must not block on forge reads (the `queue_snapshot` module doc
//! records the identical rationale), and the tick's own data does not carry
//! PR timestamps. `duration_seconds` therefore measures this process's
//! observation window: a daemon restart restarts the clock at the next tick's
//! first sight of the lock, so the attribute is a floor on the true lockout
//! age, never an overstatement. Reading it as "at least this long" is the
//! intended operator semantics.
//!
//! # Failed listings never read as a cleared lock
//!
//! Exactly like [`super::disposition::DispositionTracker::diff`]'s treatment
//! of a repo whose listing failed: a tracked repo absent from a sample
//! because its ready-issue listing errored keeps its clock. Only a
//! successfully listed tick with no `OpenPr` row clears it (and a re-lock
//! starts a fresh clock).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};

use crate::types::QueueDisposition;

/// One repo's frozen backlog, aggregated over its `OpenPr` rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrozenBacklog {
    /// Ready issues blocked from dispatch by the open-PR guard.
    pub candidates: usize,
    /// Sum of their resolved story points; unsized issues contribute
    /// nothing.
    pub points: u64,
}

/// Aggregate `rows` — `(repo key, disposition, resolved story points)`
/// triples — into per-repo frozen backlogs. Repos with no `OpenPr` row are
/// absent from the result (not present with a zero weight), so callers read
/// absence as "not locked". Pure.
#[must_use]
pub fn frozen_by_repo<'a, I>(rows: I) -> HashMap<String, FrozenBacklog>
where
    I: IntoIterator<Item = (&'a str, QueueDisposition, Option<u32>)>,
{
    let mut locked: HashMap<String, FrozenBacklog> = HashMap::new();
    for (repo, disposition, points) in rows {
        if disposition != QueueDisposition::OpenPr {
            continue;
        }
        let entry = locked.entry(repo.to_string()).or_default();
        entry.candidates = entry.candidates.saturating_add(1);
        entry.points = entry.points.saturating_add(u64::from(points.unwrap_or(0)));
    }
    locked
}

/// Per-repo first-observation clock for [`frozen_by_repo`]'s lockouts. Pure
/// core; the process-global instance lives in [`observe_global`].
#[derive(Debug, Default)]
pub struct LockoutTracker {
    since: HashMap<String, DateTime<Utc>>,
}

impl LockoutTracker {
    /// Fold one sample's lockouts in: a repo locked for the first time starts
    /// its clock at `now`; a tracked repo absent from `locked` **and** not in
    /// `failed` (its listing failed, so absence is not evidence) is cleared; a
    /// repo in `failed` keeps its clock untouched.
    pub fn observe(
        &mut self,
        locked: &HashMap<String, FrozenBacklog>,
        failed: &[String],
        now: DateTime<Utc>,
    ) {
        for slug in locked.keys() {
            self.since.entry(slug.clone()).or_insert(now);
        }
        self.since
            .retain(|slug, _| locked.contains_key(slug) || failed.iter().any(|f| f == slug));
    }

    /// The tracked lock's age in seconds at `now`, or `None` when the repo is
    /// not tracked as locked.
    #[must_use]
    pub fn duration_secs(&self, slug: &str, now: DateTime<Utc>) -> Option<i64> {
        let since = self.since.get(slug)?;
        Some(now.signed_duration_since(*since).num_seconds().max(0))
    }

    /// Whether `slug` is tracked as locked (test seam).
    #[cfg(test)]
    pub fn is_locked(&self, slug: &str) -> bool {
        self.since.contains_key(slug)
    }
}

static TRACKER: OnceLock<Mutex<LockoutTracker>> = OnceLock::new();

fn tracker() -> &'static Mutex<LockoutTracker> {
    TRACKER.get_or_init(|| Mutex::new(LockoutTracker::default()))
}

/// Fold one sample into the process-global tracker (see
/// [`LockoutTracker::observe`]). Called from the two seams that see a whole
/// tick's queue — `ops::dispatch::record_tick` (admission spans) and
/// `ops::disposition::record` (disposition spans) — so the clock starts at
/// whichever seam first observes the lock; both are idempotent
/// insert-if-absent, and only the disposition sampler's failed-listing
/// discipline clears entries.
pub fn observe_global(
    locked: &HashMap<String, FrozenBacklog>,
    failed: &[String],
    now: DateTime<Utc>,
) {
    tracker()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .observe(locked, failed, now);
}

/// The global tracker's age for `slug` at `now`, or `None` when untracked.
#[must_use]
pub fn duration_secs(slug: &str, now: DateTime<Utc>) -> Option<i64> {
    tracker()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .duration_secs(slug, now)
}

/// Test-only reset of the process-global tracker.
#[cfg(test)]
pub(crate) fn reset() {
    *tracker()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = LockoutTracker::default();
}

/// One repo's lockout as a span should report it: the frozen-backlog weight
/// plus the clock's age, already resolved against the tracker so the span
/// builders stay pure (and deterministically testable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoLockout {
    /// The frozen backlog weight.
    pub backlog: FrozenBacklog,
    /// Seconds since this process first observed the lock, when the tracker
    /// has one (always, right after [`sample`]).
    pub duration_secs: Option<i64>,
}

/// Aggregate `rows` (the same triples [`frozen_by_repo`] takes) into per-repo
/// lockouts, advance the process-global clock, and resolve each repo's
/// duration at `now` — the one call both span-export seams (`ops::dispatch::record_tick`
/// and `ops::disposition::record`) make per tick. The returned map is pure
/// data: hand it to the span builders freely.
pub fn sample<'a, I>(rows: I, failed: &[String], now: DateTime<Utc>) -> HashMap<String, RepoLockout>
where
    I: IntoIterator<Item = (&'a str, QueueDisposition, Option<u32>)>,
{
    let locked = frozen_by_repo(rows);
    observe_global(&locked, failed, now);
    locked
        .into_iter()
        .map(|(slug, backlog)| {
            let duration_secs = duration_secs(&slug, now);
            (
                slug,
                RepoLockout {
                    backlog,
                    duration_secs,
                },
            )
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "lockout_tests.rs"]
mod tests;
