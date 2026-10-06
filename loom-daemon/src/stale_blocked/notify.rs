//! The close-triggered gathering behind `loom-daemon notify-cleared-blockers`
//! (#9102), on the batched REST + ETag gatherer (#10515).
//!
//! It runs after every merge, so its cost must scale with what the merge
//! closed, not with the repo's whole `loom:blocked` population. The text of
//! every candidate comes from [`super::batch`]'s one listing plus a comment
//! walk only where there are comments (both ETag'd, shared with the sweep's
//! `check-stale-blocked` pass through the same `stale-` cache). Only the
//! artifacts that cite a just-closed number, and do not already carry that
//! number's marker, go on to the closing-PR and blocker-state reads.
//!
//! An artifact whose text read failed is always kept, so it is reported as not
//! evaluated, never silently dropped as "not cited".

use std::collections::HashMap;

use super::batch::{gather_filtered, Gathering, Options, StaleBlockedForge};
use super::{cited_among, Artifact};
use crate::dep_recheck::extract;
use crate::forge_identity::FleetLogins;

/// The idempotency marker's prefix. Rendered as `{MARKER_PREFIX}<N> -->`.
pub const MARKER_PREFIX: &str = "<!-- loom:blocker-cleared:#";

/// The idempotency marker for one closed number.
#[must_use]
pub fn marker_for(closed: i64) -> String {
    format!("{MARKER_PREFIX}{closed} -->")
}

/// Whether this artifact already carries `closed`'s marker.
#[must_use]
pub fn has_marker(input: &extract::Input, closed: i64) -> bool {
    let marker = marker_for(closed);
    input.body.contains(&marker) || input.comments.iter().any(|c| c.body.contains(&marker))
}

/// The cited-only gathering: the batch result holds only the kept artifacts,
/// and `cited` names, per artifact, the closed numbers it cites and has not
/// yet been notified about. A failed-read artifact has no `cited` entry.
pub struct CitedGathering {
    pub gathering: Gathering,
    pub cited: HashMap<(Artifact, i64), Vec<i64>>,
}

/// Gather evidence only for the open `loom:blocked` artifacts that cite a
/// number in `closed` without its marker (plus every unreadable one).
pub fn gather_cited(
    forge: &mut dyn StaleBlockedForge,
    fleet: &FleetLogins,
    opts: Options,
    closed: &[i64],
) -> CitedGathering {
    let mut cited: HashMap<(Artifact, i64), Vec<i64>> = HashMap::new();
    let gathering = gather_filtered(forge, fleet, opts, &mut |kind, number, input| {
        let hit: Vec<i64> = cited_among(kind, input, closed)
            .into_iter()
            .filter(|n| !has_marker(input, *n))
            .collect();
        if hit.is_empty() {
            return false;
        }
        cited.insert((kind, number), hit);
        true
    });
    CitedGathering { gathering, cited }
}
