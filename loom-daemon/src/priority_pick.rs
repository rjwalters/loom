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
//!    priority ([`softmax_weight`], a softmax with a temperature, #11684).
//!    The pool is three-tier: workspaces
//!    whose next issue is a verified red-main fix if any (#11547), else
//!    workspaces holding dispatchable `loom:very-important` work if any,
//!    else every workspace with dispatchable work.
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

/// The weight of the pool's top workspace (the one at `p_min`) in
/// [`softmax_weight`]. Large enough that the curve keeps ~6 decimal digits of
/// resolution before the floor of 1 bites; small enough that a sum over any
/// realistic fleet stays far inside `u64` (and inside a JS `Number`'s exact
/// integer range, which `pick.decision` readers rely on).
pub const DRAW_WEIGHT_SCALE: u64 = 1_000_000;

/// Default draw temperature, in `fleet_priority` units
/// (`dispatch.drawTemperature`, #11684). At 80, priority 20 outweighs
/// priority 200 about 9.5x — roughly the spread the pre-#11684
/// `1000 / (1 + p)` curve gave across the fleet's priority range.
pub const DEFAULT_DRAW_TEMPERATURE: u32 = 80;

/// The accepted `dispatch.drawTemperature` range. An integer on purpose: the
/// draw records derive `Eq`, and the hyperparams range check is integer-only.
pub const DRAW_TEMPERATURE_RANGE: (u64, u64) = (1, 100_000);

/// Draw weight of a workspace from its `fleet_priority` (lower number =
/// higher priority, default 100): a softmax over priorities read as negative
/// logits (#11684),
///
/// ```text
/// w(p) = max(1, round(DRAW_WEIGHT_SCALE * exp(-(p - p_min) / T)))
/// ```
///
/// `p_min` is the smallest priority in the pool being drawn over, so the top
/// workspace always weighs exactly [`DRAW_WEIGHT_SCALE`] and only priority
/// *differences* matter. `temperature` (`T`) is the one knob: small means
/// nearly strict priority order, large means nearly uniform. A `p` below
/// `p_min` is treated as `p_min`, and `T = 0` as `T = 1`, so the function is
/// total. Never 0, never above [`DRAW_WEIGHT_SCALE`].
#[must_use]
pub fn softmax_weight(priority: u32, p_min: u32, temperature: u32) -> u64 {
    let gap = f64::from(priority.saturating_sub(p_min));
    let t = f64::from(temperature.max(1));
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let w = ((DRAW_WEIGHT_SCALE as f64) * (-gap / t).exp()).round() as u64;
    w.clamp(1, DRAW_WEIGHT_SCALE)
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
    /// Whether its next candidate is a verified red-main fix (#11547).
    pub has_red_main_fix: bool,
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
    /// `"red-main-fix"` when only workspaces whose next candidate is a
    /// verified red-main fix were drawn over (#11547), else
    /// `"very-important"` when only workspaces with very-important work were,
    /// `"all"` otherwise.
    pub pool: &'static str,
    /// The candidates drawn over, sorted by workspace name, with weights.
    pub candidates: Vec<DrawCandidate>,
    /// Sum of the weights.
    pub total_weight: u64,
    /// The draw temperature the weights were computed at
    /// (`dispatch.drawTemperature`, #11684).
    pub temperature: u32,
    /// The smallest priority in the pool: the one weighing
    /// [`DRAW_WEIGHT_SCALE`] (#11684).
    pub p_min: u32,
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
/// The pool is three-tier (#11547): the entries whose next candidate is a
/// verified red-main fix if any; else those with very-important work if any;
/// else all. Within the pool the draw is weighted by [`softmax_weight`] at
/// `temperature`, with `p_min` taken over the pool (not over every entry), so
/// the pool's top workspace always weighs [`DRAW_WEIGHT_SCALE`]. Entries are
/// put in name order first, so the result depends only on the set, the
/// temperature and the RNG state.
pub fn draw_workspace(
    entries: &[WorkspaceEntry],
    temperature: u32,
    rng: &mut PickRng,
) -> Option<WorkspaceDraw> {
    let any_fix = entries.iter().any(|e| e.has_red_main_fix);
    let any_vi = entries.iter().any(|e| e.has_very_important);
    let (pool_name, in_pool): (&'static str, fn(&WorkspaceEntry) -> bool) = if any_fix {
        ("red-main-fix", |e| e.has_red_main_fix)
    } else if any_vi {
        ("very-important", |e| e.has_very_important)
    } else {
        ("all", |_| true)
    };
    let mut pool: Vec<&WorkspaceEntry> = entries.iter().filter(|e| in_pool(e)).collect();
    pool.sort_by(|a, b| a.workspace.cmp(&b.workspace));
    let p_min = pool.iter().map(|e| e.priority).min().unwrap_or(0);
    let candidates: Vec<DrawCandidate> = pool
        .iter()
        .map(|e| DrawCandidate {
            workspace: e.workspace.clone(),
            weight: softmax_weight(e.priority, p_min, temperature),
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
        pool: pool_name,
        candidates,
        total_weight,
        temperature,
        p_min,
        roll,
        picked,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "priority_pick_tests.rs"]
mod tests;
