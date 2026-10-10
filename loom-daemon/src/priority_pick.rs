//! The three-level priority model and the two-step pick (#11103).
//!
//! Loom's priority is **three levels with two labels**, the same for every
//! actor (operator, Guide, Champion):
//!
//! | Level | Label |
//! |---|---|
//! | default | *(none)* |
//! | important | `loom:important` |
//! | very important | `loom:very-important` |
//!
//! Dispatch is **workspace first, then issue**:
//!
//! 1. [`draw_workspace`] picks a workspace at random, weighted by workspace
//!    priority ([`weight_for_priority`]). A workspace holding dispatchable
//!    `loom:very-important` work wins: the draw is over just those
//!    workspaces, else over every workspace with dispatchable work.
//! 2. [`order_in_workspace`] / [`next_in_workspace`] pick the issue inside
//!    the workspace: level, then oldest `createdAt`, then issue number.
//!
//! There is no inheritance, no starred-at time, no red-main key and no
//! cross-workspace round-robin here. The module is pure: the RNG is a
//! seedable [`PickRng`], so a fixed seed reproduces an exact pick sequence,
//! and [`WorkspaceDraw`] is the record `pick.decision` carries.

use std::cmp::Ordering;

use serde::Serialize;

/// The "important" label.
pub const IMPORTANT_LABEL: &str = "loom:important";
/// The "very important" label (capped; see `daemon-reference.md`).
pub const VERY_IMPORTANT_LABEL: &str = "loom:very-important";

/// An issue's priority level. Declaration order is ascending importance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Level {
    /// No priority label.
    Default,
    /// `loom:important`.
    Important,
    /// `loom:very-important`.
    VeryImportant,
}

impl Level {
    /// The level named by `labels`: the highest of the two priority labels
    /// present, [`Level::Default`] when neither is.
    #[must_use]
    pub fn of<S: AsRef<str>>(labels: &[S]) -> Self {
        let mut level = Self::Default;
        for l in labels {
            match l.as_ref() {
                VERY_IMPORTANT_LABEL => return Self::VeryImportant,
                IMPORTANT_LABEL => level = Self::Important,
                _ => {}
            }
        }
        level
    }
}

/// One issue as the within-workspace order sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueKey {
    /// Issue number.
    pub number: u32,
    /// ISO-8601 `createdAt`; `None` sorts after every dated issue.
    pub created_at: Option<String>,
    /// Priority level.
    pub level: Level,
}

/// Total order inside one workspace: level (highest first), oldest
/// `createdAt` first (a dated issue before an undated one), issue number
/// ascending.
#[must_use]
pub fn issue_cmp(a: &IssueKey, b: &IssueKey) -> Ordering {
    b.level
        .cmp(&a.level)
        .then_with(|| match (&a.created_at, &b.created_at) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        })
        .then_with(|| a.number.cmp(&b.number))
}

/// `items` sorted by [`issue_cmp`].
#[must_use]
pub fn order_in_workspace(mut items: Vec<IssueKey>) -> Vec<IssueKey> {
    items.sort_by(issue_cmp);
    items
}

/// The next issue to dispatch in a workspace, if any.
#[must_use]
pub fn next_in_workspace(items: &[IssueKey]) -> Option<&IssueKey> {
    items.iter().min_by(|a, b| issue_cmp(a, b))
}

/// Draw weight of a workspace from its `fleet_priority` (lower number =
/// higher priority, default 100): `1000 / (1 + priority)`, at least 1. So
/// priority 0 weighs 1000, 10 weighs 90, 100 (the default) weighs 9.
#[must_use]
pub fn weight_for_priority(priority: u32) -> u64 {
    (1000 / (1 + u64::from(priority))).max(1)
}

/// A workspace with dispatchable work, as the draw sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    /// Workspace name (`owner/repo`); also the final deterministic order.
    pub workspace: String,
    /// The workspace's `fleet_priority` (lower = higher priority).
    pub priority: u32,
    /// Whether it has open, dispatchable `loom:very-important` work.
    pub has_very_important: bool,
}

/// A candidate in the draw, with its weight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DrawCandidate {
    /// Workspace name.
    pub workspace: String,
    /// Its draw weight.
    pub weight: u64,
}

/// What `pick.decision` records for one workspace draw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceDraw {
    /// `"very-important"` when only workspaces with very-important work were
    /// drawn over, `"all"` otherwise.
    pub pool: &'static str,
    /// The candidates drawn over, sorted by workspace name, with weights.
    pub candidates: Vec<DrawCandidate>,
    /// Sum of the weights.
    pub total_weight: u64,
    /// The RNG value in `0..total_weight`.
    pub roll: u64,
    /// The winning workspace.
    pub picked: String,
}

/// A small seedable PRNG (`SplitMix64`): deterministic across platforms, no
/// dependency, plenty for a weighted pick.
#[derive(Debug, Clone)]
pub struct PickRng(u64);

impl PickRng {
    /// An RNG whose sequence is fixed by `seed`.
    #[must_use]
    pub const fn from_seed(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound` (`bound` > 0).
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound.max(1)
    }
}

/// Draw the workspace to dispatch from. `None` when `entries` is empty.
///
/// A workspace with very-important work wins: when any entry has it, the
/// draw is over only those, else over all. Within the pool the draw is
/// weighted by [`weight_for_priority`]. Entries are put in name order first,
/// so the result depends only on the set and the RNG state.
pub fn draw_workspace(entries: &[WorkspaceEntry], rng: &mut PickRng) -> Option<WorkspaceDraw> {
    let any_vi = entries.iter().any(|e| e.has_very_important);
    let mut pool: Vec<&WorkspaceEntry> = entries
        .iter()
        .filter(|e| !any_vi || e.has_very_important)
        .collect();
    pool.sort_by(|a, b| a.workspace.cmp(&b.workspace));
    let candidates: Vec<DrawCandidate> = pool
        .iter()
        .map(|e| DrawCandidate {
            workspace: e.workspace.clone(),
            weight: weight_for_priority(e.priority),
        })
        .collect();
    let total_weight: u64 = candidates.iter().map(|c| c.weight).sum();
    if candidates.is_empty() {
        return None;
    }
    let roll = rng.below(total_weight);
    let mut acc = 0;
    let mut picked = candidates[0].workspace.clone();
    for c in &candidates {
        acc += c.weight;
        if roll < acc {
            picked = c.workspace.clone();
            break;
        }
    }
    Some(WorkspaceDraw {
        pool: if any_vi { "very-important" } else { "all" },
        candidates,
        total_weight,
        roll,
        picked,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "priority_pick_tests.rs"]
mod tests;
