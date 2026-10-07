//! Shadow fleet management (#10525): heuristic tiers and the shadow budget.
//!
//! Every registered heuristic of a kind is estimated on every pass and scored
//! against the same outcomes ([`super::shadow`]). That is what makes running
//! many shadows cheap. It is also why their number needs a bound, and why each
//! one has to say what it is for.
//!
//! # Tiers
//!
//! Every heuristic declares a [`Tier`] ([`super::Heuristic::tier`]):
//!
//! - [`Tier::Baseline`]: a reference every candidate is scored beside
//!   (`start-v1`, `finish-v1`, `land-v1`, the `little-v0` floor). It is
//!   estimated and shadowed like any other, but the promotion gate never
//!   promotes it.
//! - [`Tier::Candidate`]: a challenger. Only a candidate can be promoted, and
//!   only candidates are offered in the loom-ui ETA chooser (the tier rides on
//!   every `eta.snapshot` alternate).
//! - [`Tier::Retired`]: no longer registered (#10484, #10549, #10528). It produces no
//!   estimate, no alternate and no ledger pair. The id stays in [`RETIRED`]
//!   so it is never reused and a lookup still answers what it was.
//!
//! # The shadow budget
//!
//! `autonomous.eta.shadow.maxActive` ([`DEFAULT_MAX_ACTIVE`]) caps the number
//! of registered heuristics **per kind**, the current one included. A build
//! whose registry exceeds the configured budget does not start the ETA tracker
//! ([`super::Registry::check_budget`]). The refusal names the kind and the
//! heuristics past the budget, so the fix (retire one, or raise the budget) is
//! a decision someone makes, not a silent truncation. A unit test holds the
//! built-in registry within the default budget, so an over-budget registration
//! fails CI before it can reach a daemon.

use super::Kind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Default `autonomous.eta.shadow.maxActive`: registered heuristics per kind.
///
/// 13 is the kind's `current` plus the 12 alternates one `eta.snapshot` row
/// carries ([`crate::telemetry::kinds::eta_snapshot::MAX_ALTERNATES`],
/// #10549), so a registry within the default budget never has a shadow the
/// snapshot silently drops. A unit test holds the two together.
pub const DEFAULT_MAX_ACTIVE: usize = 13;

/// Smallest budget accepted. A kind always has its `current` heuristic.
pub const MIN_MAX_ACTIVE: usize = 1;

/// What a heuristic is for (#10525).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// A reference every candidate is scored beside; never promoted.
    Baseline,
    /// A challenger: promotable, and offered in the ETA chooser.
    Candidate,
    /// No longer registered; the id is kept so it is never reused.
    Retired,
}

impl Tier {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Baseline => "baseline",
            Tier::Candidate => "candidate",
            Tier::Retired => "retired",
        }
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Retired heuristic ids and their kind (#10484, #10549, #10528). Never
/// registered, never reused; see `eta.md` "Retired heuristics" for why each
/// was retired. `land-2026-10-04-twin-otter`'s evaluation lives on inside
/// `land-2026-10-04-twin-otter-b` (and keen-wren's PR stages); only its own
/// registration is retired.
pub const RETIRED: &[(&str, Kind)] = &[
    (super::heuristics::LAND_V3, Kind::Land),
    ("land-2026-10-04-amber-heron", Kind::Land),
    ("land-2026-10-04-fresh-tide", Kind::Land),
    (super::heuristics::LAND_TWIN_OTTER, Kind::Land),
];

/// Whether `id` is a retired heuristic id.
#[must_use]
pub fn is_retired(id: &str) -> bool {
    RETIRED.iter().any(|(retired, _)| *retired == id)
}

/// A registry with more heuristics of one kind than the shadow budget allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetExceeded {
    /// The first kind over budget.
    pub kind: Kind,
    /// The configured budget.
    pub max_active: usize,
    /// How many heuristics of that kind are registered.
    pub registered: usize,
    /// The ones past the budget, in registration order.
    pub excess: Vec<&'static str>,
}

impl std::fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} heuristics are registered but autonomous.eta.shadow.maxActive is {}; \
             over the budget: {} (retire a heuristic or raise the budget)",
            self.registered,
            self.kind,
            self.max_active,
            self.excess.join(", ")
        )
    }
}

impl std::error::Error for BudgetExceeded {}

/// Check registered ids, grouped by kind in registration order, against
/// `max_active` per kind. The first kind over budget (in the order given) is
/// the error. Pure.
///
/// # Errors
///
/// A kind registers more than `max_active` heuristics.
pub fn check_budget<I>(by_kind: I, max_active: usize) -> Result<(), BudgetExceeded>
where
    I: IntoIterator<Item = (Kind, Vec<&'static str>)>,
{
    for (kind, ids) in by_kind {
        if ids.len() > max_active {
            return Err(BudgetExceeded {
                kind,
                max_active,
                registered: ids.len(),
                excess: ids[max_active..].to_vec(),
            });
        }
    }
    Ok(())
}

/// The tier of a heuristic id this build knows: a built-in registration's
/// declared tier, [`Tier::Retired`] for a [`RETIRED`] id, `None` for anything
/// else. Tiers are compile-time facts, independent of any coefficient file, so
/// the built-in registry answers for every registry this build constructs.
#[must_use]
pub fn builtin_tier(id: &str) -> Option<Tier> {
    static TIERS: OnceLock<BTreeMap<&'static str, Tier>> = OnceLock::new();
    TIERS
        .get_or_init(|| {
            let registry = super::Registry::builtin();
            registry
                .ids()
                .into_iter()
                .filter_map(|id| registry.tier_of(id).map(|tier| (id, tier)))
                .collect()
        })
        .get(id)
        .copied()
        .or_else(|| is_retired(id).then_some(Tier::Retired))
}
