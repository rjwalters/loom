//! `loom-daemon operator-decision` (#9344): the one helper every role uses to
//! open or relabel a `loom:operator-decision` issue.
//!
//! An operator decision is not free prose. It is a one-line question, a line
//! or two of context, and **2-4 options ranked best → worst**, each carrying a
//! *why* (what it wins, what it gives up against the options above it). The
//! first option is the recommended one. The canonical encoding is a fenced
//! ```` ```decision ```` JSON block in the issue body — the shape the
//! downstream dashboard (2AMLogic/loom-ui) validates, bouncing anything else
//! back to its author as `loom:decision-malformed`. The convention is
//! `/repo:decide` (rjwalters/repo#486); the reference doc is
//! `defaults/docs/operator-decision.md`.
//!
//! This module owns three things, kept in separate files so each is testable
//! on its own:
//!
//! - [`validate`] — the contract. Every failure is a named [`validate::Reason`]
//!   and ALL of them are reported, never just the first, so an author fixes
//!   the input in one pass.
//! - [`render`] — the fenced block plus the readable ranked list, and the
//!   idempotent body composition that replaces a prior decision section
//!   instead of stacking a second one, keeping the original report under
//!   `## Original report`.
//! - [`cli`] — `validate` and `apply`. `apply` refuses (non-zero, nothing
//!   touched) on any contract failure, writes the body before it applies any
//!   label, and with `--dry-run` issues no mutation at all.

use serde::{Deserialize, Serialize};

pub mod cli;
pub mod render;
pub mod validate;

#[cfg(test)]
mod tests;

/// The operator-decision label this helper is the sole sanctioned applier of.
pub const DECISION_LABEL: &str = "loom:operator-decision";

/// The label loom-ui swaps in when a decision fails the contract. A
/// successful `apply` clears it: a valid body is the repair.
pub const MALFORMED_LABEL: &str = "loom:decision-malformed";

/// Fewest options a decision may offer: one option is a directive, not a
/// decision.
pub const MIN_OPTIONS: usize = 2;

/// Most options a decision may offer: loom-ui votes with keys 1-4.
pub const MAX_OPTIONS: usize = 4;

/// One ranked option. Fields are lenient on input (`#[serde(default)]`) so a
/// missing field surfaces as a named contract reason from
/// [`validate::validate`] rather than as an opaque JSON parse error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionOption {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// `None` = the key is absent (`missing_why`); `Some("")` or whitespace
    /// = present but empty (`empty_why`). The two are different author
    /// mistakes, so they get different reason codes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// The decision payload: the helper's input and the fenced block's content.
///
/// `options` order IS the ranking (best first); `recommended` must equal
/// `options[0].id`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub context: String,
    #[serde(default)]
    pub options: Vec<DecisionOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_links: Vec<String>,
}
