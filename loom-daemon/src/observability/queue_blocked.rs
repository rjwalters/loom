//! Where a workspace's issue listing comes from: the tick's published
//! snapshot (mirror mode, #12307), or a listing of its own.
//!
//! This module used to build the forge-side `loom:blocked` rows of the native
//! `queue.snapshot` record (Issue #8957); that record was removed with the
//! `/ingest` exporter (#12813). What remains is the snapshot-or-list choice
//! `stage_occupancy` makes per workspace.

use std::path::Path;

use crate::forge_etag_store::{host_scope, Target};
use crate::forge_listing::{listing_target, RestIssue};
use crate::forge_repo_facts::Fact;

/// Where one workspace's blocked listing comes from.
pub(super) enum Rows {
    /// The tick's snapshot (mirror mode, #12307); nothing is listed.
    Snapshot(Vec<RestIssue>),
    /// No data for the expected identity. This is unknown, like a failed
    /// listing, including when the checkout now points at a different repo.
    Skip,
    /// Not mirror mode: list this exact host and repo on every page.
    List(Target),
}

pub(super) fn snapshot_rows(root: &Path, slug: &str, fact: Option<&Fact>) -> Rows {
    use crate::discovery::published::{lookup, Lookup};
    let matches = |host: &str, name: &str| match fact {
        // Repo facts verified the checkout's configured name and the
        // canonical name on this exact host. Accept either spelling only
        // when that proof belongs to the listing host.
        Some(fact) => {
            host.eq_ignore_ascii_case(&fact.host)
                && (name.eq_ignore_ascii_case(slug)
                    || name.eq_ignore_ascii_case(&fact.configured_nwo))
        }
        // Without a canonical fact, there is no proof of an alias. An
        // exact slug is still usable on the current explicitly resolved
        // target, whose host is carried separately through the whole read.
        None => name.eq_ignore_ascii_case(slug),
    };
    match lookup(root) {
        // Mirror snapshots are keyed by name and carry no stable repo id.
        // A configured alias can have changed hands since it was verified.
        Lookup::Repo(repo)
            if repo.slug.eq_ignore_ascii_case(slug)
                && fact.is_none_or(|fact| fact.host.eq_ignore_ascii_case("github.com")) =>
        {
            Rows::Snapshot(repo.rows.clone())
        }
        Lookup::Repo(_) | Lookup::Absent => Rows::Skip,
        Lookup::NotMirror => {
            let mut target = listing_target(Some(root), None);
            let host = host_scope(&target);
            let Some(name) = target.repo.as_deref() else {
                return Rows::Skip;
            };
            if !matches(&host, name) {
                return Rows::Skip;
            }
            // For a proved rename or transfer, ask for the canonical
            // identity directly, rather than following a stale redirect.
            // Keep the existing cache key when only ASCII case differs.
            if !name.eq_ignore_ascii_case(slug) {
                target.repo = Some(slug.to_owned());
            }
            target.host = Some(host);
            Rows::List(target)
        }
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "queue_blocked_tests.rs"]
mod tests;
