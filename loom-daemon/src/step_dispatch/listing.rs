//! The step engine's all-open listing (#11894 §4.2 step 1, #11897).
//!
//! A step tick classifies every open item of a repo, so it lists them all,
//! whatever their labels. Two sources, tried in order, and no new discovery
//! seam (#11894 §3: #11845's `Option` fallback is the seam):
//!
//! 1. the GitHub state mirror, through
//!    [`crate::forge_mirror::discovery::list_open`] as the step-engine caller
//!    with no label, when `forge.mirror` is live and every page it serves is
//!    fresh for the repo. Its rows carry `labels_at`, the first stage-entry
//!    source ([`super::stage_entry`]);
//! 2. otherwise [`crate::forge_listing::list_open_issues_cached_all_as`], the
//!    ETag-cached REST walk the intake pass and the stage-queue gauge already
//!    read: an idle repo costs only free `304`s.
//!
//! The mirror read is always a
//! [`ReadKind::Discovery`](crate::forge_events::poll_gate::ReadKind::Discovery)
//! read: a claim, a label transition or a merge re-reads the forge itself.

use std::path::Path;

use anyhow::Result;

use crate::forge_listing::RestIssue;

/// The `caller` the REST walk is accounted under in `loom.forge.calls`, the
/// same token the mirror read uses.
pub const CALLER: &str = "step_engine";

/// Which source answered a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingSource {
    /// The GitHub state mirror.
    Mirror,
    /// The ETag-cached REST walk.
    Forge,
}

impl ListingSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mirror => "mirror",
            Self::Forge => "forge",
        }
    }
}

/// Every open issue and pull request of one repo, oldest first, and where
/// the rows came from.
#[derive(Debug)]
pub struct OpenItems {
    pub rows: Vec<RestIssue>,
    pub source: ListingSource,
}

/// Every open item of the repo at `cwd` (or `repo`): the mirror when it
/// vouches for the repo, else the shared REST walk.
///
/// # Errors
///
/// The REST walk's: a page failed or was shed, the walk passed its page cap,
/// or the listing moved mid-walk. The set is all-or-nothing, so a caller must
/// not read a missing item as absent.
pub fn list_all_open(gh_bin: &Path, cwd: Option<&Path>, repo: Option<&str>) -> Result<OpenItems> {
    list_all_open_with(cwd, repo, mirror_rows, |cwd, repo| {
        crate::forge_listing::list_open_issues_cached_all_as(CALLER, gh_bin, cwd, repo)
    })
}

/// [`list_all_open`] with both sources given, so the order is testable
/// offline.
///
/// # Errors
///
/// `forge`'s, when the mirror did not answer.
pub fn list_all_open_with(
    cwd: Option<&Path>,
    repo: Option<&str>,
    mirror: impl FnOnce(Option<&Path>, Option<&str>) -> Option<Vec<RestIssue>>,
    forge: impl FnOnce(Option<&Path>, Option<&str>) -> Result<Vec<RestIssue>>,
) -> Result<OpenItems> {
    if let Some(rows) = mirror(cwd, repo) {
        return Ok(OpenItems {
            rows,
            source: ListingSource::Mirror,
        });
    }
    Ok(OpenItems {
        rows: forge(cwd, repo)?,
        source: ListingSource::Forge,
    })
}

/// The all-open listing from the GitHub state mirror, or `None` (read the
/// forge): no mirror live, the repo stale or unknown to it, or any error.
fn mirror_rows(cwd: Option<&Path>, repo: Option<&str>) -> Option<Vec<RestIssue>> {
    use crate::forge_mirror::discovery::{list_open, Caller, Pages};
    if !crate::forge_mirror::is_active() {
        return None;
    }
    list_open(
        crate::forge_events::poll_gate::ReadKind::Discovery,
        Caller::StepEngine,
        cwd,
        repo,
        None,
        Pages::All,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "listing_tests.rs"]
mod tests;
