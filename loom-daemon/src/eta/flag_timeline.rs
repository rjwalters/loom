//! The label-flag timeline (#10245): when each PR's six model flags
//! ([`super::labels::pr_flags`]) changed, stored next to its stage episodes
//! in the fleet snapshot so a fit can read the flags **as they were** at any
//! past instant. Before this, past flags were stored nowhere: the full label
//! stream existed only transiently while a snapshot was merged.
//!
//! # Derivation
//!
//! [`flag_changes_from_input`] replays a PR's label events exactly as the
//! stage episodes do (one shared helper, `episodes::replay`): `(at, seq)`
//! order, every event at one instant applied before the flags are resolved,
//! nothing at or after a merge or close, only `at < as_of`. It emits one
//! [`FlagChange`] at the PR's **first** label event, even when no flag is set,
//! then one per instant at which the mask changes. The unconditional first
//! entry is what tells "never flagged" apart from "recorded before the
//! timeline existed": a PR with episodes and no entry is the second.
//!
//! # Knowability
//!
//! Entries store the label events' own instants; a reader applies its own
//! lag ([`flags_before`] takes the cutoff).

use super::episodes::{replay, EpisodeInput};
use super::labels::pr_flags;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One instant at which a PR's flag mask took a new value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FlagChange {
    /// The PR.
    pub pr_number: u32,
    /// The label event's instant.
    pub at: DateTime<Utc>,
    /// The mask in force from `at` on ([`super::labels::pr_flags`]).
    pub flags: u8,
}

impl FlagChange {
    /// The one-line canonical form a snapshot id is digested over.
    #[must_use]
    pub fn digest_line(&self) -> String {
        format!(
            "flags|{}|{}|{}",
            self.pr_number,
            crate::telemetry::trace::instant(self.at),
            self.flags
        )
    }
}

/// One [`FlagChange`] with the repo it belongs to: how a flag timeline
/// reaches an estimator ([`super::history::StageSamples::flag_changes`],
/// #10523). A fleet snapshot is per repo, so its changes carry no repo of
/// their own; a history merges several snapshots, so each change needs one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RepoFlagChange {
    /// `owner/repo`.
    pub repo: String,
    /// The change.
    pub change: FlagChange,
}

/// `input`'s flag timeline as knowable at `as_of`: an entry at its first label
/// event, then one per change of the mask.
#[must_use]
pub fn flag_changes_from_input(input: &EpisodeInput, as_of: DateTime<Utc>) -> Vec<FlagChange> {
    let mut out: Vec<FlagChange> = Vec::new();
    replay(input, as_of, |at, present| {
        let flags = pr_flags(present);
        if out.last().is_none_or(|last| last.flags != flags) {
            out.push(FlagChange {
                pr_number: input.pr_number,
                at,
                flags,
            });
        }
    });
    out
}

/// Retention: every change at or after `floor`, plus each PR's last change
/// before it, so the mask in force **at** the floor survives the prune.
#[must_use]
pub fn prune(changes: Vec<FlagChange>, floor: DateTime<Utc>) -> Vec<FlagChange> {
    let mut in_force: BTreeMap<u32, FlagChange> = BTreeMap::new();
    let mut kept = Vec::with_capacity(changes.len());
    for change in changes {
        if change.at >= floor {
            kept.push(change);
        } else {
            let slot = in_force.entry(change.pr_number).or_insert(change);
            if (change.at, change.flags) > (slot.at, slot.flags) {
                *slot = change;
            }
        }
    }
    kept.extend(in_force.into_values());
    kept
}

/// The mask in force strictly before `cutoff` from one PR's changes (any
/// order), or `None` when none is that old.
#[must_use]
pub fn flags_before(changes: &[FlagChange], cutoff: DateTime<Utc>) -> Option<u8> {
    changes
        .iter()
        .filter(|c| c.at < cutoff)
        .max_by_key(|c| (c.at, c.flags))
        .map(|c| c.flags)
}
