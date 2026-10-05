//! The per-PR walk of the raw fleet event cache (#10197): reviews and check
//! runs.
//!
//! The issue-events and pulls listings are repo-wide: one paged walk covers
//! every item. Reviews are per PR (`pulls/{n}/reviews`) and check runs per
//! head commit (`commits/{sha}/check-runs`), so their cost scales with the
//! number of PRs. This module bounds it:
//!
//! - **Only what is not cached yet.** The work list ([`pr_work`]) is derived
//!   from the cache itself — the PRs it holds, their open/closed state and
//!   (for check runs) the head commit the pulls listing recorded. A **closed**
//!   PR is walked once, unconditionally and to the end of its listing, and
//!   then *settled*: a marker row (the endpoint's event kind, no `label`,
//!   `seq` 0, stamped at the PR's close time) is appended, and the PR is never
//!   read again. A PR reopened and closed again has a new close time and is
//!   read once more. An **open** PR is re-read whenever a run reaches it, page 1
//!   conditional on its cached ETag, so a quiet PR costs a `304`.
//! - **One page budget per endpoint.** `max_pages` is shared by every PR of
//!   one endpoint in one run; each PR costs at least one page.
//! - **A rotating start.** Pending PRs (open, and unsettled closed) are
//!   walked newest-numbered first as a cycle: the endpoint's ledger entry
//!   records the PR whose walk last completed (`resume_after`), and the next
//!   run starts after it, wrapping. A run never restarts at the same open PRs,
//!   so with any budget every pending PR is reached within
//!   `ceil(pages needed / max_pages)` runs and no PR starves.
//!
//! Each PR's walk is an ordinary [`sync`] of the same [`RawEventSource`],
//! under its own cursor key (`forge:reviews#<n>`,
//! `forge:check-runs#<n>@<sha>`), so the page checkpoint, the ETag and the
//! rate-limit / reserve stop are exactly the repo-wide endpoints'. The key of
//! a settled PR is dropped from the cursor (and any key the work list no
//! longer names — a head that moved — is dropped at the start of a run), so
//! the cursor holds only open and in-progress PRs. The endpoint's
//! `forge:<endpoint>` entry keeps the call ledger (`pages_fetched`).
//!
//! A kill at any point resumes byte-identically: the work list and its order
//! are recomputed from the same file, a settled PR is skipped, and the PR in
//! progress resumes at its checkpointed page. Marker rows carry the run's
//! `now` as `fetched_at`, never part of the id.
//!
//! # Known limits
//!
//! - Only the **last** head commit the pulls listing showed is read for a
//!   closed PR, so check runs of earlier pushes are not cached. Every row
//!   names its commit, and [`super::fleet_state_prs`] reads only the head at
//!   `t`'s runs, so an instant before that head was recorded is unknown,
//!   never "no CI" and never another head's verdict.
//! - An open PR's conditional refresh can miss reviews or runs past a full
//!   first page (over 100); its settle walk after close reads everything.
//! - A review submitted after the PR closed is read only if it landed before
//!   the settle walk.

use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::fleet_events::{
    sync, EndpointCursor, EventKind, EventLog, EventsCursor, ItemKind, RawEvent, RawEventSource,
    SyncMode, SyncOutcome, SOURCE_FORGE,
};

/// The cursor/CLI name of the reviews endpoint.
pub const REVIEWS: &str = "reviews";

/// The cursor/CLI name of the check-runs endpoint.
pub const CHECK_RUNS: &str = "check-runs";

/// Which per-PR listing a walk reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerPrKind {
    Reviews,
    CheckRuns,
}

impl PerPrKind {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            PerPrKind::Reviews => REVIEWS,
            PerPrKind::CheckRuns => CHECK_RUNS,
        }
    }

    /// The kind of the rows (and of the settle marker) this listing yields.
    #[must_use]
    pub fn event_kind(self) -> EventKind {
        match self {
            PerPrKind::Reviews => EventKind::Review,
            PerPrKind::CheckRuns => EventKind::CheckRun,
        }
    }

    /// The endpoint's ledger key, `forge:<name>`.
    #[must_use]
    pub fn ledger_key(self) -> String {
        format!("{SOURCE_FORGE}:{}", self.name())
    }

    /// The cursor key of one PR's walk.
    #[must_use]
    pub fn item_key(self, pr: u32, sha: Option<&str>) -> String {
        match self {
            PerPrKind::Reviews => format!("{}#{pr}", self.ledger_key()),
            PerPrKind::CheckRuns => {
                format!("{}#{pr}@{}", self.ledger_key(), sha.unwrap_or(""))
            }
        }
    }
}

/// One PR as the cache knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrWork {
    pub pr: u32,
    /// When it (last) closed; `None` while open.
    pub closed_at: Option<DateTime<Utc>>,
    /// The latest head commit the pulls listing recorded.
    pub head: Option<String>,
    /// Reviews read after its last close.
    pub reviews_settled: bool,
    /// Check runs read after its last close.
    pub checks_settled: bool,
}

impl PrWork {
    fn settled(&self, kind: PerPrKind) -> bool {
        match kind {
            PerPrKind::Reviews => self.reviews_settled,
            PerPrKind::CheckRuns => self.checks_settled,
        }
    }

    /// Whether `kind` still has to be read for this PR.
    #[must_use]
    pub fn pending(&self, kind: PerPrKind) -> bool {
        !self.settled(kind) && (kind == PerPrKind::Reviews || self.head.is_some())
    }
}

/// Every PR of `repo` in `events`, with what is still to be read.
///
/// Only [`SOURCE_FORGE`] rows count: the fan-out reads the forge, so its work
/// list (which PRs exist, when they closed, which walks are settled) is derived
/// from forge rows alone. Imported rows from other sources (e.g. the webhook
/// mirror) carry their own receipt times and PRs outside the forge window;
/// letting them in would move `closed_at` off the settle markers and queue
/// PRs the forge never listed (#10197).
#[must_use]
pub fn pr_work(events: &[RawEvent], repo: &str) -> Vec<PrWork> {
    let mut rows: Vec<&RawEvent> = events
        .iter()
        .filter(|e| {
            e.source == SOURCE_FORGE
                && e.item_kind == ItemKind::Pr
                && e.repo.eq_ignore_ascii_case(repo)
        })
        .collect();
    rows.sort_by(|a, b| a.canonical_key().cmp(&b.canonical_key()));
    let mut prs: BTreeMap<u32, PrWork> = BTreeMap::new();
    let mut markers: BTreeSet<(u32, EventKind, DateTime<Utc>)> = BTreeSet::new();
    for e in rows {
        let pr = prs.entry(e.item).or_insert_with(|| PrWork {
            pr: e.item,
            closed_at: None,
            head: None,
            reviews_settled: false,
            checks_settled: false,
        });
        match e.kind {
            EventKind::Closed | EventKind::Merged => pr.closed_at = Some(e.event_time),
            EventKind::Reopened => pr.closed_at = None,
            EventKind::HeadCommit => pr.head.clone_from(&e.label),
            EventKind::Review | EventKind::CheckRun if e.label.is_none() => {
                markers.insert((e.item, e.kind, e.event_time));
            }
            _ => {}
        }
    }
    prs.into_values()
        .map(|mut pr| {
            if let Some(at) = pr.closed_at {
                pr.reviews_settled = markers.contains(&(pr.pr, EventKind::Review, at));
                pr.checks_settled = markers.contains(&(pr.pr, EventKind::CheckRun, at));
            }
            pr
        })
        .collect()
}

/// The marker that settles `kind` for a PR closed at `closed_at`.
#[must_use]
pub fn settle_marker(
    repo: &str,
    pr: u32,
    kind: PerPrKind,
    closed_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> RawEvent {
    RawEvent::new(repo, pr, ItemKind::Pr, kind.event_kind(), None, closed_at, SOURCE_FORGE, 0, now)
}

/// A source that reads one PR's listing at a time.
pub trait PerPrSource: RawEventSource {
    /// Point the next requests at PR `pr` (and, for check runs, `sha`).
    fn select(&mut self, pr: u32, sha: Option<&str>);
}

/// The pending PRs of `work` in the order this run visits them: newest-numbered
/// first, as a cycle that starts after the ledger's `resume_after` (the PR
/// whose walk last completed) and wraps. A PR interrupted mid-walk is not
/// passed, so the next run resumes it first; a completed one moves to the
/// back. Every pending PR is thus reached within a bounded number of runs,
/// whatever `max_pages` is.
fn visit_order<'a>(work: &'a [PrWork], kind: PerPrKind, cursor: &EventsCursor) -> Vec<&'a PrWork> {
    let mut pending: Vec<&PrWork> = work.iter().filter(|w| w.pending(kind)).collect();
    pending.sort_by_key(|w| std::cmp::Reverse(w.pr));
    let after = cursor
        .endpoints
        .get(&kind.ledger_key())
        .and_then(|c| c.resume_after);
    if let Some(after) = after {
        let start = pending.iter().position(|w| w.pr < after).unwrap_or(0);
        pending.rotate_left(start);
    }
    pending
}

/// What a [`sync_per_pr`] run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanoutReport {
    pub outcome: SyncOutcome,
    /// Page requests made this run (including `304`s).
    pub pages: u64,
    /// Rows appended this run (markers included).
    pub appended: usize,
    /// Closed PRs settled this run.
    pub settled: usize,
    /// Closed PRs still unsettled after this run.
    pub remaining: usize,
}

/// Walk `kind` for every pending PR of `work`, in order, within `max_pages`
/// requests in total, checkpointing to `cursor_file` after every page.
///
/// # Errors
///
/// Appending to the log or writing the cursor failed. Source failures end the
/// run with [`SyncOutcome::Stopped`].
#[allow(clippy::too_many_arguments)]
pub fn sync_per_pr(
    kind: PerPrKind,
    source: &mut dyn PerPrSource,
    work: &[PrWork],
    log: &mut EventLog,
    cursor: &mut EventsCursor,
    cursor_file: &Path,
    max_pages: u64,
    now: DateTime<Utc>,
) -> std::io::Result<FanoutReport> {
    let pending = visit_order(work, kind, cursor);
    let mut remaining = pending.iter().filter(|w| w.closed_at.is_some()).count();

    // Drop keys the work list no longer names (settled, or a moved head).
    let live: BTreeSet<String> = pending
        .iter()
        .map(|w| kind.item_key(w.pr, w.head.as_deref()))
        .collect();
    let prefix = format!("{}#", kind.ledger_key());
    let before = cursor.endpoints.len();
    cursor
        .endpoints
        .retain(|k, _| !k.starts_with(&prefix) || live.contains(k));
    if cursor.endpoints.len() != before {
        cursor.write(cursor_file)?;
    }

    let mut report = FanoutReport {
        outcome: SyncOutcome::Complete,
        pages: 0,
        appended: 0,
        settled: 0,
        remaining,
    };
    for w in pending {
        if report.pages >= max_pages {
            report.outcome = SyncOutcome::PageBudget;
            break;
        }
        source.select(w.pr, w.head.as_deref());
        let key = source.cursor_key();
        let entry = cursor.endpoints.get(&key);
        let walked = entry.is_some_and(|c| c.backfill_complete);
        let settling = w.closed_at.is_some() && entry.and_then(|c| c.settle_for) == w.closed_at;
        let mode = match w.closed_at {
            // Closed: read once more, in full and unconditionally. A settle
            // walk already under way (or done) continues where it stopped.
            Some(at) => {
                if !settling {
                    let fresh = EndpointCursor {
                        settle_for: Some(at),
                        ..EndpointCursor::default()
                    };
                    cursor.endpoints.insert(key.clone(), fresh);
                }
                SyncMode::Backfill
            }
            None if walked => SyncMode::Refresh,
            None => SyncMode::Backfill,
        };
        let run = sync(source, log, cursor, cursor_file, mode, max_pages - report.pages)?;
        report.pages += run.pages;
        report.appended += run.appended;
        cursor
            .endpoints
            .entry(kind.ledger_key())
            .or_default()
            .pages_fetched += run.pages;
        if run.outcome == SyncOutcome::Complete {
            cursor
                .endpoints
                .entry(kind.ledger_key())
                .or_default()
                .resume_after = Some(w.pr);
            if let Some(closed_at) = w.closed_at {
                let marker = settle_marker(&cursor.repo, w.pr, kind, closed_at, now);
                report.appended += log.append(&[marker])?;
                cursor.endpoints.remove(&key);
                report.settled += 1;
                remaining -= 1;
            }
        }
        cursor.write(cursor_file)?;
        if run.outcome != SyncOutcome::Complete {
            report.outcome = run.outcome;
            break;
        }
    }
    report.remaining = remaining;
    Ok(report)
}

#[cfg(test)]
#[path = "fleet_events_fanout_tests.rs"]
mod tests;
