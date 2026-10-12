//! Side-listing filters and the host's cap terms (#9244, re-scoped by
//! #11508).
//!
//! Priority (`loom:important` / `loom:very-important`, #11103) changes
//! dispatch **order only, never eligibility** (operator ruling on #11508):
//! there is no priority side listing, so a priority issue is dispatched only
//! through the normal `loom:issue` path, and no priority label exempts a
//! candidate from any gate (build back-off, file overlap, the caps).
//!
//! What remains here:
//!
//! - [`excluded_from_side_listing`]: the filter the unpromoted red-main-fix
//!   listing (#10118) applies to rows from outside `loom:issue`.
//! - [`CapTerms`]: the configured vs headroom terms of the concurrency cap.
//! - [`OverflowSlot`]: **inert** since #11508. Nothing is admitted over the
//!   cap any more; the slot only reports whether an overflow sweep started
//!   by an older daemon is still live (`plan.slots.overflow_free`).
//!
//! The legacy star readers that `star_liveness/` (#11104) and
//! `forge_priority_labels` (#11510) still use live in
//! `crate::star_liveness::dispatch_shim` and are re-exported below so their
//! import paths are unchanged; dispatch does not call them.

use super::{WorkDispatcher, WorkItem, BUILDING_LABEL};

pub use crate::star_liveness::dispatch_shim::{
    merge_starred, GhTimelineStarredAt, StarredAtSource,
};

/// The legacy operator-priority ("starred") label (#9244). Dispatch no
/// longer reads it (#11508); kept for the non-dispatch readers #11510 and
/// #11104 remove.
pub const OPERATOR_PRIORITY_LABEL: &str = "loom:operator-priority";

/// The in-progress Curator claim. A side-listed row carrying it is being
/// curated right now, so it is left alone this tick.
pub const CURATING_LABEL: &str = "loom:curating";

/// Labels that route an issue through Champion rather than the Builder: an
/// epic, and the three proposal kinds. A side-listed row carrying one is not
/// build work, so it is skipped. Derived from the label registry's
/// `champion_path` property (#10013).
pub static CHAMPION_PATH_LABELS: crate::label_registry::LabelSet =
    crate::label_registry::LabelSet::new(|| crate::label_registry::embedded_set("champion_path"));

/// Whether a row from a listing other than `loom:issue` (the unpromoted
/// red-main-fix ones, #10118) is left out of the candidates: already claimed
/// ([`BUILDING_LABEL`]), being curated ([`CURATING_LABEL`]), or on the
/// Champion path ([`CHAMPION_PATH_LABELS`]).
#[must_use]
pub fn excluded_from_side_listing(item: &WorkItem) -> bool {
    item.labels.iter().any(|l| {
        l == BUILDING_LABEL || l == CURATING_LABEL || CHAMPION_PATH_LABELS.contains(&l.as_str())
    })
}

/// The two kinds of term in this tick's concurrency cap (#9244 §5, #5270).
///
/// The dynamic cap is `min(disk headroom, ram headroom, configured)`: the
/// configured term is the operator's queue limit, the headroom terms say the
/// host cannot hold another worktree. Both are published on the tick summary
/// (Issue #10214) so an alert can say which term holds the cap down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapTerms {
    /// The configured `maxConcurrent` (after env / config resolution).
    pub configured: usize,
    /// `min(disk headroom, ram headroom)`: how many sweeps the host can hold.
    pub headroom: usize,
}

impl CapTerms {
    /// The terms from this tick's configured cap and its disk / RAM probes.
    #[must_use]
    pub fn new(configured: usize, disk_headroom: usize, ram_headroom: usize) -> Self {
        Self {
            configured,
            headroom: disk_headroom.min(ram_headroom),
        }
    }

    /// [`Self::new`] for a production tick: also records the raw terms for
    /// the published tick summary (Issue #10214), so the liveness pass and
    /// the fleet alert can say which term holds the cap down.
    #[must_use]
    pub fn observed(configured: usize, disk_headroom: usize, ram_headroom: usize) -> Self {
        super::tick_summary::record_cap(crate::types::CapView::from_terms(
            configured,
            disk_headroom,
            ram_headroom,
        ));
        Self::new(configured, disk_headroom, ram_headroom)
    }

    /// The effective cap every non-overflow admission is held to.
    #[must_use]
    pub fn effective(&self) -> usize {
        self.configured.min(self.headroom)
    }
}

impl From<usize> for CapTerms {
    /// A cap with no known resource headroom: every term is configured. The
    /// plain `tick*` entry points use this; the production loops build the
    /// real terms from their disk and RAM probes.
    fn from(configured: usize) -> Self {
        Self {
            configured,
            headroom: usize::MAX,
        }
    }
}

/// The per-host overflow slot (#9244 §5), **inert since #11508**.
///
/// It used to let one starred candidate run as this host's single over-limit
/// sweep. Priority no longer changes admission (operator ruling on #11508),
/// so nothing takes the slot; it is kept only so the tick still reports
/// whether an overflow sweep an older daemon started is live
/// ([`Self::is_free`], `plan.slots.overflow_free`) and so the effective cap
/// comes from one place ([`Self::open`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverflowSlot {
    taken: bool,
}

impl OverflowSlot {
    /// The tick's effective cap plus its slot, for the single-workspace tick;
    /// `already_live` is whether an overflow sweep is still running.
    #[must_use]
    pub fn open(already_live: bool, terms: CapTerms) -> (usize, Self) {
        (
            terms.effective(),
            Self {
                taken: already_live,
            },
        )
    }

    /// [`Self::open`] for the multi-workspace tick: the slot is taken when
    /// any workspace already has a live overflow sweep.
    #[must_use]
    pub fn for_workspaces<S, D: WorkDispatcher>(
        workspaces: &[(S, D)],
        terms: CapTerms,
    ) -> (usize, Self) {
        let live = workspaces.iter().any(|(_, d)| d.overflow_in_flight());
        Self::open(live, terms)
    }

    /// Whether no overflow sweep is live (Issue #9288's
    /// `plan.slots.overflow_free`).
    #[must_use]
    pub fn is_free(&self) -> bool {
        !self.taken
    }
}
