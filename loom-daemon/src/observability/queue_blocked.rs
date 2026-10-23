//! Where a workspace's issue listing comes from: the tick's published
//! snapshot (mirror mode, #12307), or a listing of its own.
//!
//! This module used to build the forge-side `loom:blocked` rows of the native
//! `queue.snapshot` record (Issue #8957); that record was removed with the
//! `/ingest` exporter (#12813). What remains is the snapshot-or-list choice
//! `stage_occupancy` makes per workspace.

use std::path::Path;

use crate::forge_listing::RestIssue;

/// Where one workspace's blocked listing comes from.
pub(super) enum Rows {
    /// The tick's snapshot (mirror mode, #12307); nothing is listed.
    Snapshot(Vec<RestIssue>),
    /// Mirror mode, and the snapshot has not read the repo: no rows, like a
    /// failed listing.
    Skip,
    /// Not mirror mode: list it.
    List,
}

pub(super) fn snapshot_rows(root: &Path) -> Rows {
    use crate::discovery::published::{lookup, Lookup};
    match lookup(root) {
        Lookup::Repo(repo) => Rows::Snapshot(repo.rows.clone()),
        Lookup::Absent => Rows::Skip,
        Lookup::NotMirror => Rows::List,
    }
}
