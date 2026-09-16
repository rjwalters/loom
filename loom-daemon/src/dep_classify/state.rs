//! Reference state resolution — the Rust port of
//! `classify-dependency-block.sh`'s `_ref_state` and `_classify_refs`
//! (epic #7810, PR 3).
//!
//! Sorts a set of declared blockers into **open**, **resolved** and **unknown**.
//! The three-way split is the whole point: only a fully-resolved set clears a
//! defer, and an *unknown* reference must never be silently counted as resolved
//! — that would un-escalate a proposal whose blocker is merely unreadable.
//!
//! # What changes versus the shell
//!
//! The original asked `gh` for `--json state --jq '.state'`, so it received a
//! bare string in which "this is not an issue", "gh failed", and "the field was
//! empty" were all the same empty value:
//!
//! ```text
//! st="$("$GH_READ" issue view "$num" … --jq '.state' 2>/dev/null)"
//! if [[ -z "$st" ]]; then st="$(…pr view…)"; fi
//! printf '%s\n' "${st:-UNKNOWN}"
//! ```
//!
//! The fallback to `pr view` is *correct* — a reference may name a PR rather
//! than an issue — but it fires on any empty answer, including a forge outage,
//! and then a second failure yields `UNKNOWN`. The outcome was right; the
//! reason was unknowable.
//!
//! Routed through [`crate::cmd_out`] (PR 2) the reasons are now distinct, so the
//! PR fallback fires on a genuine *not-found* and a forge failure is recorded as
//! such. The resulting classification is unchanged — `UNKNOWN` either way —
//! which is what keeps the 252-assertion shell suite green.

use crate::cmd_out::Query;
use crate::script_helpers::gh_query;
use serde::Deserialize;
use std::path::Path;

/// `gh … --json state` → `{"state":"OPEN"}`.
#[derive(Debug, Deserialize)]
struct StateField {
    #[serde(default)]
    state: String,
}

/// Where a declared blocker stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefState {
    /// Still open: the block genuinely holds.
    Open,
    /// Closed or merged: the stated reason for the block is gone.
    Resolved,
    /// Could not be determined. **Never** treated as resolved.
    Unknown,
}

impl RefState {
    /// Map a forge `state` string, matching the shell's `case`.
    ///
    /// Anything that is not `OPEN`, `CLOSED` or `MERGED` — including an empty
    /// string — is `Unknown`, so an unrecognised future state fails safe rather
    /// than clearing a block.
    fn from_forge(s: &str) -> Self {
        match s {
            "OPEN" => RefState::Open,
            "CLOSED" | "MERGED" => RefState::Resolved,
            _ => RefState::Unknown,
        }
    }
}

/// The state of one `owner/repo#N` node, asking the forge.
///
/// Tries `issue view` first, then `pr view` — a reference may name either. The
/// fallback now fires only on a genuine *empty result*, not on any failure.
#[must_use]
pub fn ref_state(node: &str, repo_root: &Path, use_cache: bool) -> RefState {
    let Some((repo, num)) = node.rsplit_once('#') else {
        return RefState::Unknown;
    };

    for entity in ["issue", "pr"] {
        let q: Query<StateField> = gh_query(
            &[entity, "view", num, "--repo", repo, "--json", "state"],
            repo_root,
            use_cache,
            |s: &StateField| s.state.is_empty(),
        );
        match q {
            Query::Populated(s) => return RefState::from_forge(&s.state),
            // Not this entity kind — try the other. This is the case the shell
            // reached via an empty string, and the only one that should fall
            // through.
            Query::Empty => continue,
            // Ran and refused, or could not be asked. `gh issue view` on a PR
            // number exits non-zero, so Failed must also fall through to the
            // `pr view` attempt or every PR reference would read as Unknown.
            Query::Failed { .. } | Query::Malformed { .. } | Query::Unavailable(_) => continue,
        }
    }

    RefState::Unknown
}

/// A set of blockers split by state.
///
/// Field order mirrors the shell's `OPEN_REFS` / `RESOLVED_REFS` /
/// `UNKNOWN_REFS`, and each preserves the input order so the rendered output is
/// stable.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClassifiedRefs {
    pub open: Vec<String>,
    pub resolved: Vec<String>,
    pub unknown: Vec<String>,
}

impl ClassifiedRefs {
    /// Space-joined, as the shell stored them.
    #[must_use]
    pub fn open_joined(&self) -> String {
        self.open.join(" ")
    }

    #[must_use]
    pub fn resolved_joined(&self) -> String {
        self.resolved.join(" ")
    }

    #[must_use]
    pub fn unknown_joined(&self) -> String {
        self.unknown.join(" ")
    }
}

/// Split `nodes` (one per line) by state, using `lookup` to resolve each.
///
/// The lookup is injected so the classification logic is testable without a
/// forge — the shell could only be exercised through a `gh` stub on `PATH`.
pub fn classify_refs_with<F>(nodes: &str, mut lookup: F) -> ClassifiedRefs
where
    F: FnMut(&str) -> RefState,
{
    let mut out = ClassifiedRefs::default();
    for node in nodes.lines() {
        let node = node.trim();
        if node.is_empty() {
            continue;
        }
        match lookup(node) {
            RefState::Open => out.open.push(node.to_string()),
            RefState::Resolved => out.resolved.push(node.to_string()),
            RefState::Unknown => out.unknown.push(node.to_string()),
        }
    }
    out
}

/// Split `nodes` by state, asking the forge for each.
#[must_use]
pub fn classify_refs(nodes: &str, repo_root: &Path, use_cache: bool) -> ClassifiedRefs {
    classify_refs_with(nodes, |node| ref_state(node, repo_root, use_cache))
}

#[cfg(test)]
mod tests;
