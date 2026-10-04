//! Observed-versus-inventoried operation diff (Issue #9831).
//!
//! The call-identity layer ([`crate::forge_call_stats`]) records the
//! inventoried operation ID of every migrated forge call in the per-host sink.
//! This module sets those observed IDs beside the inventory and reports both
//! directions of disagreement:
//!
//! - **inventoried but never observed** — an active row no recorded call
//!   exercised in the sink's retention window. Not a defect by itself: the
//!   operation may be rare, served by a script (scripts do not write the sink),
//!   or its call site may not be migrated yet.
//! - **observed but not inventoried** — an ID the sink saw that names no
//!   inventory row at all. Always worth a look: either the inventory is missing
//!   a row or a call site names a typo.
//!
//! `unknown` is reported on its own line with the callers that produced it —
//! the list of call sites still to map.
//!
//! **Runtime traces supplement the source inventory; they never establish
//! exhaustiveness.** A sink shows what one host *did* call in a few hours, not
//! what Loom *can* call (#9777). Nothing here may be read as "the inventory is
//! complete" — that claim belongs to the source-side gate and validator.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::forge_call_stats::{ObservedOperation, UNKNOWN_OPERATION};
use crate::forge_inventory::model::Inventory;

/// The caveat every rendering of this diff carries, verbatim.
pub const EXHAUSTIVENESS_CAVEAT: &str =
    "runtime traces supplement the source inventory and never prove \
     exhaustiveness: an operation absent from this sink may still be called, and a \
     clean diff does not mean the inventory is complete";

/// One observed operation ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObservedRow {
    pub id: String,
    pub calls: u64,
    pub callers: Vec<String>,
    /// The matching row's disposition, when the ID is inventoried. A
    /// `prohibition` here means a prohibited operation was actually called.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disposition: Option<&'static str>,
}

/// An active inventory row no recorded call exercised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnobservedRow {
    pub id: String,
    pub disposition: &'static str,
}

/// The whole diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObservedDiff {
    pub caveat: &'static str,
    /// Inventoried IDs the sink observed.
    pub observed_and_inventoried: Vec<ObservedRow>,
    /// Active inventory rows the sink never observed.
    pub inventoried_never_observed: Vec<UnobservedRow>,
    /// Observed IDs naming no inventory row.
    pub observed_not_inventoried: Vec<ObservedRow>,
    /// Calls recorded as `unknown`, and by which callers.
    pub unknown: Option<ObservedRow>,
}

fn observed_row(
    id: &str,
    seen: &ObservedOperation,
    disposition: Option<&'static str>,
) -> ObservedRow {
    ObservedRow {
        id: id.to_string(),
        calls: seen.calls,
        callers: seen.callers.iter().cloned().collect(),
        disposition,
    }
}

/// Diff the observed operation IDs against `inv`.
#[must_use]
pub fn diff(inv: &Inventory, observed: &BTreeMap<String, ObservedOperation>) -> ObservedDiff {
    let rows: BTreeMap<&str, &crate::forge_inventory::model::Operation> =
        inv.operations.iter().map(|o| (o.id.as_str(), o)).collect();

    let mut observed_and_inventoried = Vec::new();
    let mut observed_not_inventoried = Vec::new();
    let mut unknown = None;
    for (id, seen) in observed {
        if id == UNKNOWN_OPERATION {
            unknown = Some(observed_row(id, seen, None));
        } else if let Some(op) = rows.get(id.as_str()) {
            observed_and_inventoried.push(observed_row(id, seen, Some(op.disposition.as_str())));
        } else {
            observed_not_inventoried.push(observed_row(id, seen, None));
        }
    }

    let inventoried_never_observed = inv
        .operations
        .iter()
        .filter(|o| o.is_active() && !observed.contains_key(&o.id))
        .map(|o| UnobservedRow {
            id: o.id.clone(),
            disposition: o.disposition.as_str(),
        })
        .collect();

    ObservedDiff {
        caveat: EXHAUSTIVENESS_CAVEAT,
        observed_and_inventoried,
        inventoried_never_observed,
        observed_not_inventoried,
        unknown,
    }
}

/// Human-readable rendering.
#[must_use]
pub fn render_text(diff: &ObservedDiff) -> String {
    let mut out = String::new();
    out.push_str("forge operations: observed (host call sink) vs inventoried\n");
    out.push_str(&format!("  NOTE: {}.\n", diff.caveat));

    out.push_str(&format!(
        "\nOBSERVED AND INVENTORIED ({})\n",
        diff.observed_and_inventoried.len()
    ));
    for r in &diff.observed_and_inventoried {
        let flag = match r.disposition {
            Some(d @ ("prohibition" | "test-fixture")) => format!("  [{d} row CALLED]"),
            _ => String::new(),
        };
        out.push_str(&format!("  {:<34} {:>6} call(s){flag}\n", r.id, r.calls));
    }

    out.push_str(&format!(
        "\nOBSERVED BUT NOT INVENTORIED ({})\n",
        diff.observed_not_inventoried.len()
    ));
    if diff.observed_not_inventoried.is_empty() {
        out.push_str("  none\n");
    }
    for r in &diff.observed_not_inventoried {
        out.push_str(&format!(
            "  {:<34} {:>6} call(s)  callers: {}\n",
            r.id,
            r.calls,
            r.callers.join(", ")
        ));
    }

    out.push_str(&format!(
        "\nINVENTORIED BUT NEVER OBSERVED ({})\n",
        diff.inventoried_never_observed.len()
    ));
    for r in &diff.inventoried_never_observed {
        out.push_str(&format!("  {:<34} {}\n", r.id, r.disposition));
    }

    out.push_str("\nUNMAPPED (`unknown`)\n");
    match &diff.unknown {
        Some(u) => out.push_str(&format!("  {} call(s) from: {}\n", u.calls, u.callers.join(", "))),
        None => out.push_str("  none recorded\n"),
    }
    out
}
