//! PR debt per repository: how many open PRs wait for review, for changes
//! and for merge (issue #9392's ledger, fed by step dispatch since #11911).
//!
//! The ledger maps `(root, axis)` to the number of open PRs last seen in that
//! root's queue for the axis, and when. Two readers act on it:
//!
//! - the work finder's **build back-off** (#9410, #10624,
//!   [`crate::work_finder::build_backoff`]): while a repository's finished
//!   work piles up, Builder steps there wait;
//! - the **pipeline-empty idle gate** (#10817,
//!   [`crate::role_runner::idle_gate`]): Hermit and Architect generate only
//!   when the repository has nothing in flight.
//!
//! **Where the counts come from.** Until #11911 the role runner fed this from
//! its Judge and Doctor queue-gate listings and one `loom:pr` count per
//! Champion run, and read it back to size those roles' width and a
//! Champion-first reservation (`autonomous.roleRunner.demandWidth`). Judge,
//! Doctor and Champion's merges are steps now, so the role runner neither
//! lists those queues nor sizes anything from them. The step tick's
//! all-open listing ([`crate::step_dispatch::gather`]) already holds every
//! open PR with its labels, so it records all three axes for its root
//! ([`record_open_listing`]) with no forge call of its own.
//!
//! **Freshness.** An entry older than [`STALE_AFTER`] is unobserved: a root
//! the step tick has not listed recently reads `None` on that axis, never
//! `0`, so both readers fail open. A failed listing records nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crate::forge_listing::RestIssue;

/// Entries older than this are unobserved. The old
/// `autonomous.roleRunner.demandWidth.staleSecs` default, now fixed: the step
/// tick lists a drawn repository every few minutes, well inside it.
pub const STALE_AFTER: Duration = Duration::from_secs(1800);

/// One PR queue whose depth the readers act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DebtAxis {
    /// `loom:review-requested`: Judge steps.
    Review,
    /// `loom:changes-requested`: Doctor steps.
    Changes,
    /// `loom:pr`: Champion merge steps.
    Merge,
}

impl DebtAxis {
    /// Every axis.
    pub const ALL: [Self; 3] = [Self::Review, Self::Changes, Self::Merge];

    /// The label whose open PRs this axis counts.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Review => "loom:review-requested",
            Self::Changes => "loom:changes-requested",
            Self::Merge => "loom:pr",
        }
    }

    /// The axis a queue label feeds, if any.
    #[must_use]
    pub fn for_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.label() == label)
    }
}

/// One axis summed across the roots with a fresh entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AxisDebt {
    /// Open PRs across the fresh entries.
    pub total: usize,
    /// Fresh entries with a nonzero count.
    pub roots_with_debt: usize,
}

/// The debt a host (or one repository) carries; `None` is an unobserved axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostDebt {
    /// `loom:review-requested`.
    pub review: Option<AxisDebt>,
    /// `loom:changes-requested`.
    pub changes: Option<AxisDebt>,
    /// `loom:pr`.
    pub merge: Option<AxisDebt>,
}

impl HostDebt {
    /// The debt on `axis`.
    #[must_use]
    pub fn axis(&self, axis: DebtAxis) -> Option<AxisDebt> {
        match axis {
            DebtAxis::Review => self.review,
            DebtAxis::Changes => self.changes,
            DebtAxis::Merge => self.merge,
        }
    }

    fn axis_mut(&mut self, axis: DebtAxis) -> &mut Option<AxisDebt> {
        match axis {
            DebtAxis::Review => &mut self.review,
            DebtAxis::Changes => &mut self.changes,
            DebtAxis::Merge => &mut self.merge,
        }
    }
}

type LedgerEntries = HashMap<(PathBuf, DebtAxis), (usize, Instant)>;

/// The PR-debt ledger (module doc). Tests build their own with
/// [`DebtLedger::default`] so they never share state.
#[derive(Debug, Default)]
pub struct DebtLedger {
    entries: Mutex<LedgerEntries>,
    /// Debt reads, so a test can assert a disabled reader never reads.
    reads: AtomicUsize,
}

impl DebtLedger {
    /// Record `count` open PRs on `axis` for `root`, observed now.
    pub fn record(&self, root: &Path, axis: DebtAxis, count: usize) {
        self.record_at(root, axis, count, Instant::now());
    }

    /// [`Self::record`] with an explicit observation time.
    pub fn record_at(&self, root: &Path, axis: DebtAxis, count: usize, at: Instant) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.insert((root.to_path_buf(), axis), (count, at));
    }

    /// The host aggregate over entries no older than `stale`.
    #[must_use]
    pub fn host_debt(&self, stale: Duration) -> HostDebt {
        self.host_debt_at(Instant::now(), stale)
    }

    /// [`Self::host_debt`] as of `now`.
    #[must_use]
    pub fn host_debt_at(&self, now: Instant, stale: Duration) -> HostDebt {
        self.debt_at(None, now, stale)
    }

    /// `root`'s own debt over its entries no older than `stale` (#10624).
    #[must_use]
    pub fn repo_debt(&self, root: &Path, stale: Duration) -> HostDebt {
        self.repo_debt_at(root, Instant::now(), stale)
    }

    /// [`Self::repo_debt`] as of `now`.
    #[must_use]
    pub fn repo_debt_at(&self, root: &Path, now: Instant, stale: Duration) -> HostDebt {
        self.debt_at(Some(root), now, stale)
    }

    fn debt_at(&self, only: Option<&Path>, now: Instant, stale: Duration) -> HostDebt {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let mut debt = HostDebt::default();
        for ((_, axis), (count, at)) in entries
            .iter()
            .filter(|((r, _), _)| only.is_none_or(|o| o == r))
        {
            if now.saturating_duration_since(*at) > stale {
                continue;
            }
            let slot = debt.axis_mut(*axis).get_or_insert_with(AxisDebt::default);
            slot.total += count;
            slot.roots_with_debt += usize::from(*count > 0);
        }
        debt
    }

    /// Drop the entries of roots no longer in the registry.
    pub fn retain_roots(&self, roots: &[PathBuf]) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|(root, _), _| roots.contains(root));
    }

    /// How many times a debt has been read.
    #[must_use]
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }
}

/// The process-wide ledger the step tick feeds and the readers read.
#[must_use]
pub fn global() -> &'static DebtLedger {
    static LEDGER: OnceLock<DebtLedger> = OnceLock::new();
    LEDGER.get_or_init(DebtLedger::default)
}

/// Labels that make a `loom:pr` PR one Champion will not merge: held for a
/// human (#9410). Derived (#11795): [`crate::work_finder::HOLD_NAMES`], the
/// three holds and the retired names that fold into them. `loom:frozen`
/// (#11743) is one of them.
pub use crate::work_finder::HOLD_NAMES as MERGE_HOLD_LABELS;

/// Labels whose presence means `axis`'s role will not act on the PR:
/// [`MERGE_HOLD_LABELS`] for merge, [`crate::work_finder::PARK_LABELS`] (the
/// set `pr-queue` drops for Judge and Doctor, #9421, #11743, #11861) for
/// review and changes. `loom:treating` is a live claim, not a park, so it
/// stays counted.
#[must_use]
pub fn axis_park_labels(axis: DebtAxis) -> &'static [&'static str] {
    match axis {
        DebtAxis::Merge => &MERGE_HOLD_LABELS,
        DebtAxis::Changes | DebtAxis::Review => &crate::work_finder::PARK_LABELS,
    }
}

/// Open PR rows that count as debt on `axis` (the caller passes rows of the
/// axis's queue). Parked and held PRs are left out ([`axis_park_labels`]):
/// counting work no role will drain would keep the build back-off engaged.
/// One exception (#11861): a Champion hold out at Doctor, or handed back to
/// Judge, is that role's work; its `pr-queue` confirms it from the trusted
/// hold marker before acting.
#[must_use]
pub fn count_axis_rows(axis: DebtAxis, rows: &[RestIssue]) -> usize {
    let parks = axis_park_labels(axis);
    rows.iter()
        .filter(|r| r.is_pull_request && r.state.eq_ignore_ascii_case("open"))
        .filter(|r| {
            !r.labels.iter().any(|l| parks.contains(&l.as_str())) || champion_round_trip(axis, r)
        })
        .count()
}

/// The listing-only hint that a parked PR is a Champion hold on its round
/// trip (#11861).
fn champion_round_trip(axis: DebtAxis, r: &RestIssue) -> bool {
    let shape = match axis {
        DebtAxis::Changes => crate::champion_hold::doctor_shape(&r.labels),
        DebtAxis::Review => crate::champion_hold::judge_shape(&r.labels),
        DebtAxis::Merge => false,
    };
    shape
        && r.body
            .as_deref()
            .is_some_and(crate::champion_hold::body_declares_hold)
}

/// Record every axis for `root` from one complete listing of its open items
/// (issues and PRs together, as the step tick reads them). Each axis counts
/// the rows carrying its label ([`count_axis_rows`]); an axis with no such
/// row is recorded as `0`, observed.
pub fn record_open_listing(ledger: &DebtLedger, root: &Path, rows: &[RestIssue]) {
    for axis in DebtAxis::ALL {
        let queue: Vec<RestIssue> = rows
            .iter()
            .filter(|r| r.labels.iter().any(|l| l == axis.label()))
            .cloned()
            .collect();
        ledger.record(root, axis, count_axis_rows(axis, &queue));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pr_debt_tests.rs"]
mod tests;
