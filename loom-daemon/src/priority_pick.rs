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
//! 1. [`draw_workspace`] picks a workspace at random with probability
//!    P = w / Σw (#11742), where w is the workspace's base `weight` from the
//!    fleet roster plus [`WeightBonus`] points while it holds a verified
//!    red-main fix (#11547), `loom:very-important` work or `loom:important`
//!    work. There are no pool tiers: urgent work makes a workspace more
//!    likely to be drawn, never certain.
//! 2. [`order_in_workspace`] / [`next_in_workspace`] pick the issue inside
//!    the workspace: verified red-main fix first, then level, then oldest
//!    `createdAt`, then issue number.
//!
//! `loom:very-important` is capped per workspace ([`VERY_IMPORTANT_CAP`],
//! #11509): [`very_important_holders`] names the oldest open one, which keeps
//! the level; every other open one ranks as `loom:important`.
//!
//! A verified red-main fix is an issue carrying the `loom:main-red-fix`
//! marker whose repo's `main` is verified red this tick; on a green `main`
//! the marker grants nothing. It reorders only: no gate or [`Level`] effect.
//!
//! There is no inheritance, no starred-at time and no cross-workspace
//! round-robin here. The module is pure: the RNG is a
//! seedable [`PickRng`], so a fixed seed reproduces an exact pick sequence,
//! and [`WorkspaceDraw`] is the record `pick.decision` carries.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

/// The "important" label.
pub const IMPORTANT_LABEL: &str = "loom:important";
/// The "very important" label (capped: [`VERY_IMPORTANT_CAP`]).
pub const VERY_IMPORTANT_LABEL: &str = "loom:very-important";

/// How many open `loom:very-important` issues one workspace holds at a time
/// (#11509, operator ruling C on #11278). Every further one ranks as
/// `loom:important`. A constant on purpose, not a config knob.
pub const VERY_IMPORTANT_CAP: usize = 1;

/// An issue's priority level. Declaration order is ascending importance.
/// On the wire (status, queue snapshot, fleet state) it is `default` /
/// `important` / `very-important`; an unknown value reads as `default`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Level {
    /// No priority label.
    #[default]
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

    /// The wire name: `default`, `important` or `very-important`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Important => "important",
            Self::VeryImportant => "very-important",
        }
    }

    /// The level named `wire` ([`Self::as_str`]); anything else is
    /// [`Level::Default`].
    #[must_use]
    pub fn from_wire(wire: &str) -> Self {
        match wire {
            "important" => Self::Important,
            "very-important" => Self::VeryImportant,
            _ => Self::Default,
        }
    }

    /// The integer form some wire surfaces use (`fleet-state/v1` `level`,
    /// the plan's `level` key, the PR queue): 0 default, 1 important,
    /// 2 very important.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::Important => 1,
            Self::VeryImportant => 2,
        }
    }
}

impl<'de> Deserialize<'de> for Level {
    /// Lenient: an unknown name (a newer daemon's level) reads as `default`.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self::from_wire(&String::deserialize(d)?))
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
    /// A verified red-main fix (marker + repo `main` verified red this tick,
    /// #11547): sorts ahead of every level. Never an input to the
    /// very-important cap.
    pub main_red_fix: bool,
}

/// Total order inside one workspace: verified red-main fix first (#11547),
/// then level (highest first), oldest
/// `createdAt` first (a dated issue before an undated one), issue number
/// ascending.
#[must_use]
pub fn issue_cmp(a: &IssueKey, b: &IssueKey) -> Ordering {
    b.main_red_fix
        .cmp(&a.main_red_fix)
        .then_with(|| b.level.cmp(&a.level))
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

/// The open `loom:very-important` issues of one workspace that keep the
/// level: the [`VERY_IMPORTANT_CAP`] oldest, by `createdAt` (a dated issue
/// before an undated one) then issue number. `open` is every open
/// very-important issue of the workspace, in any state; its `level` fields
/// are ignored and a number listed twice counts once. Nothing about who
/// applied the label is an input, so the cap is the same for every actor.
#[must_use]
pub fn very_important_holders(open: &[IssueKey]) -> Vec<u32> {
    let mut by_number: std::collections::BTreeMap<u32, IssueKey> =
        std::collections::BTreeMap::new();
    for k in open {
        let entry = by_number.entry(k.number).or_insert_with(|| k.clone());
        if entry.created_at.is_none() {
            entry.created_at.clone_from(&k.created_at);
        }
    }
    let mut keys: Vec<IssueKey> = by_number
        .into_values()
        .map(|k| IssueKey {
            level: Level::VeryImportant,
            main_red_fix: false,
            ..k
        })
        .collect();
    keys.sort_by(issue_cmp);
    keys.into_iter()
        .take(VERY_IMPORTANT_CAP)
        .map(|k| k.number)
        .collect()
}

/// Default bonus a workspace's weight gains while its next candidate is a
/// verified red-main fix (`dispatch.weightBonusRedMainFix`, #11742). Large
/// on purpose: a red `main` blocks every merge in that repo, so against a
/// fleet whose base weights sum to a few hundred a lone red repo takes about
/// half the draws. An initial value, retuned from measurement.
pub const DEFAULT_BONUS_RED_MAIN_FIX: u32 = 400;
/// Default bonus while the workspace has dispatchable `loom:very-important`
/// work (`dispatch.weightBonusVeryImportant`, #11742).
pub const DEFAULT_BONUS_VERY_IMPORTANT: u32 = 100;
/// Default bonus while the workspace has dispatchable `loom:important` work
/// (`dispatch.weightBonusImportant`, #11742).
pub const DEFAULT_BONUS_IMPORTANT: u32 = 20;
/// The accepted range of each weight bonus. An integer on purpose: the draw
/// records derive `Eq`, and the hyperparams range check is integer-only.
pub const WEIGHT_BONUS_RANGE: (u64, u64) = (0, 1_000_000);

/// The bonus points a workspace's weight gains while it holds each kind of
/// prioritised work (#11742, operator ruling 2026-10-13 on 2AMLogic/2am#4344).
/// They replace the old hard pool tiers: nothing wins the draw outright, a
/// workspace with urgent work is just more likely to be drawn. Bonuses stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WeightBonus {
    /// Added while the workspace's next candidate is a verified red-main fix.
    pub red_main_fix: u32,
    /// Added while it has dispatchable `loom:very-important` work.
    pub very_important: u32,
    /// Added while it has dispatchable `loom:important` work.
    pub important: u32,
}

impl Default for WeightBonus {
    fn default() -> Self {
        Self {
            red_main_fix: DEFAULT_BONUS_RED_MAIN_FIX,
            very_important: DEFAULT_BONUS_VERY_IMPORTANT,
            important: DEFAULT_BONUS_IMPORTANT,
        }
    }
}

impl WeightBonus {
    /// The bonus `entry` earns: the sum of every bonus whose condition holds.
    #[must_use]
    pub fn for_entry(&self, entry: &WorkspaceEntry) -> u64 {
        let on = |cond: bool, points: u32| if cond { u64::from(points) } else { 0 };
        on(entry.has_red_main_fix, self.red_main_fix)
            + on(entry.has_very_important, self.very_important)
            + on(entry.has_important, self.important)
    }
}

/// A workspace with dispatchable work, as the draw sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    /// Workspace name (`owner/repo`); also the final deterministic order.
    pub workspace: String,
    /// The workspace's base draw `weight` from the fleet roster. 0 means the
    /// workspace is never drawn, whatever bonuses it would earn.
    pub weight: u32,
    /// Whether its next candidate is a verified red-main fix (#11547).
    pub has_red_main_fix: bool,
    /// Whether it has dispatchable `loom:very-important` work.
    pub has_very_important: bool,
    /// Whether it has dispatchable `loom:important` work.
    pub has_important: bool,
}

/// A candidate in the draw, with its weight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DrawCandidate {
    /// Workspace name.
    pub workspace: String,
    /// Its base `weight` from the roster.
    pub base_weight: u32,
    /// The bonus points it earned this draw ([`WeightBonus::for_entry`]).
    pub bonus: u64,
    /// Its effective draw weight: `base_weight + bonus`.
    pub weight: u64,
}

/// What `pick.decision` records for one workspace draw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceDraw {
    /// The candidates drawn over (every entry with a non-zero base weight),
    /// sorted by workspace name, with weights.
    pub candidates: Vec<DrawCandidate>,
    /// Sum of the effective weights.
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

/// Draw the workspace to dispatch from: P(w) = effective weight / Σ effective
/// weights (#11742), where a workspace's effective weight is its base
/// `weight` plus the [`WeightBonus`] points it earns. There are no pool
/// tiers. A workspace with base weight 0 is never drawn. `None` when no entry
/// has a non-zero weight. Entries are put in name order first, so the result
/// depends only on the set, the bonuses and the RNG state.
pub fn draw_workspace(
    entries: &[WorkspaceEntry],
    bonus: &WeightBonus,
    rng: &mut PickRng,
) -> Option<WorkspaceDraw> {
    let mut pool: Vec<&WorkspaceEntry> = entries.iter().filter(|e| e.weight > 0).collect();
    pool.sort_by(|a, b| a.workspace.cmp(&b.workspace));
    let candidates: Vec<DrawCandidate> = pool
        .iter()
        .map(|e| {
            let extra = bonus.for_entry(e);
            DrawCandidate {
                workspace: e.workspace.clone(),
                base_weight: e.weight,
                bonus: extra,
                weight: u64::from(e.weight) + extra,
            }
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let total_weight: u64 = candidates.iter().map(|c| c.weight).sum();
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
