//! Which repos this host lists for `fleet.state` under pass sharding (#11301).
//!
//! The collector pass reads five label listings per managed repo (the three
//! review labels, `loom:curated`, `loom:triage`), and they say the same thing
//! on every host. With `autonomous.passShard` on, only the repo's owner host
//! ([`crate::pass_shard::Verdict::Owner`], or an unsharded verdict) reads
//! them. Held rows, `ready_wait` rows and `main_ci` are this host's own,
//! cost no forge call, and are unchanged for every repo.
//!
//! # Handover is "stopped observing", never "left"
//!
//! A repo this host no longer owns must not read as its PRs leaving review:
//! the outcome diff would read each vanished row as a stage exit (a forge read
//! and a false `eta.stage_outcome`), and so would treating the repo as gone
//! from `managed`. So before a collector pass builds and diffs its view,
//! [`forget`] drops each unowned repo's review-sourced rows, and its listing
//! memory, from the previous view and from the outcome memory. The rows then
//! go out once in the next record's `removed` (the emitted view still has
//! them), and no outcome is emitted. When the repo comes back, its rows are
//! first seen mid-stage (`entered_at_lower_bound`), like any new listing.
//!
//! A backup host does not list either: an intermittent second view would carry
//! lower-bound entry instants beside the owner's exact ones, and readers take
//! any host's PR-stage row.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::{FleetView, PassState, Source};

/// `roots` split into the roots this host lists and the slugs it does not.
pub(super) fn split(roots: &[(PathBuf, String)]) -> (Vec<(PathBuf, String)>, BTreeSet<String>) {
    split_with(roots, |slug| crate::pass_shard::decide_slug(slug).runs())
}

/// [`split`] with the verdict injected. Pure.
pub(super) fn split_with(
    roots: &[(PathBuf, String)],
    lists: impl Fn(&str) -> bool,
) -> (Vec<(PathBuf, String)>, BTreeSet<String>) {
    let mut listed = Vec::new();
    let mut unowned = BTreeSet::new();
    for (root, slug) in roots {
        if lists(slug) {
            listed.push((root.clone(), slug.clone()));
        } else {
            unowned.insert(slug.clone());
        }
    }
    (listed, unowned)
}

/// Drop `unowned` repos' review-sourced rows from `view`, and their review
/// observation. Held and ready rows stay. Pure.
pub(super) fn forget_view(view: &mut FleetView, unowned: &BTreeSet<String>) {
    for repo in unowned {
        if let Some(state) = view.repos.get_mut(repo) {
            let review: Vec<u32> = state
                .sources
                .iter()
                .filter(|(_, s)| **s == Source::Review)
                .map(|(n, _)| *n)
                .collect();
            for issue in review {
                state.sources.remove(&issue);
                state.rows.remove(&issue);
            }
            state.census = None;
        }
        if let Some(seen) = view.observed.get_mut(&Source::Review) {
            seen.remove(repo);
        }
    }
}

/// [`forget_view`] over everything a collector pass diffs against: the
/// previous view (carry-forward and entry instants) and the outcome memory's
/// view and review listings. The emitted view is left alone, so the next
/// record sends the rows as `removed`.
pub(super) fn forget(state: &mut PassState, unowned: &BTreeSet<String>) {
    forget_parts(state.prev.as_mut(), &mut state.outcomes, unowned);
}

/// [`forget`] over its parts. Pure.
pub(super) fn forget_parts(
    prev: Option<&mut FleetView>,
    memory: &mut super::outcomes::Memory,
    unowned: &BTreeSet<String>,
) {
    if unowned.is_empty() {
        return;
    }
    if let Some(prev) = prev {
        forget_view(prev, unowned);
    }
    if let Some(view) = memory.view.as_mut() {
        forget_view(view, unowned);
    }
    memory.listed.retain(|repo, _| !unowned.contains(repo));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "shard_scope_tests.rs"]
mod tests;
