//! The deterministic per-repository lane rule (#10630).
//!
//! For judge (review debt) and doctor (changes-requested debt) a repository
//! holds `clamp(ceil(debt / laneK), 1, cap)` concurrent runs
//! ([`super::demand::repo_lanes`]). This module turns every observed
//! repository's wish into one **host plan** that stays under the role's
//! capacity (`min(host ceiling, role budget)`), and records it so "why only
//! one Doctor?" is answered by one `pick.decision` field.
//!
//! **Trim order** (pure, [`allocate`]): when the wishes sum past the capacity,
//! repositories are ranked by `(debt ascending, repository path ascending)`.
//! Extra lanes (above the first) are removed from the front of that ranking
//! first, a repository's extras all going before the next repository is
//! touched; only if every repository is at one lane and the sum is still over
//! are whole repositories dropped to zero, in the same order. Admission
//! ([`lanes_for`]) never starves a repository's *first* lane on this ranking
//! (the classic fairness walk and the host limits already arbitrate those), so
//! it applies the trim to extra lanes only.

use super::demand::{self, DebtAxis, DemandConfig, DemandLedger};
use std::path::{Path, PathBuf};

/// One repository's lane computation for one role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoLane {
    /// The repository's workspace root.
    pub root: PathBuf,
    /// Its debt on the role's axis (last-known).
    pub debt: usize,
    /// Lanes the formula wants.
    pub wanted: usize,
    /// Lanes after the host-capacity trim.
    pub lanes: usize,
}

/// Apply the trim order (module doc) to `wishes` of `(root, debt, wanted)`
/// against `capacity`. The result is sorted by root, so it is independent of
/// the input order.
#[must_use]
pub fn allocate(wishes: &[(PathBuf, usize, usize)], capacity: usize) -> Vec<RepoLane> {
    let mut out: Vec<RepoLane> = wishes
        .iter()
        .map(|(root, debt, wanted)| RepoLane {
            root: root.clone(),
            debt: *debt,
            wanted: *wanted,
            lanes: *wanted,
        })
        .collect();
    out.sort_by(|a, b| a.root.cmp(&b.root));
    // Lowest debt first; the sort is stable over the path-sorted list, so a
    // tie falls to the lexicographically smaller path.
    let mut order: Vec<usize> = (0..out.len()).collect();
    order.sort_by_key(|&i| out[i].debt);
    let mut total: usize = out.iter().map(|r| r.lanes).sum();
    for floor in [1, 0] {
        for &i in &order {
            if total <= capacity {
                return out;
            }
            let cut = (total - capacity).min(out[i].lanes.saturating_sub(floor));
            out[i].lanes -= cut;
            total -= cut;
        }
    }
    out
}

/// The plan for `role` across every repository the ledger has observed on its
/// axis, trimmed to `capacity`. Empty for roles with no lane rule.
#[must_use]
pub fn plan_for(
    role: &str,
    ledger: &DemandLedger,
    cfg: &DemandConfig,
    capacity: usize,
) -> Vec<RepoLane> {
    let axis = match role {
        "judge" => DebtAxis::Review,
        "doctor" => DebtAxis::Changes,
        _ => return Vec::new(),
    };
    let wishes: Vec<(PathBuf, usize, usize)> = ledger
        .known_roots()
        .into_iter()
        .filter_map(|root| {
            let repo = ledger.repo_debt(&root, cfg.stale());
            let debt = repo.axis_width(axis)?;
            Some((root, debt, demand::repo_lanes(role, &repo, cfg)))
        })
        .collect();
    allocate(&wishes, capacity)
}

/// The lanes `root` may hold under `plan`: its trimmed allocation, never below
/// the classic one, and `wanted` as-is when the plan does not know the root.
#[must_use]
pub fn lanes_for(plan: &[RepoLane], root: &Path, wanted: usize) -> usize {
    plan.iter()
        .find(|r| r.root == root)
        .map_or(wanted, |r| r.lanes)
        .max(1)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "tests/lane_rule.rs"]
mod tests;
