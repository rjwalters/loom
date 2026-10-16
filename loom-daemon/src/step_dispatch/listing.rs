//! The Direct backend's all-open listing (#11894 §4.2 step 1, #11897).
//!
//! A step tick classifies every open item of a repo, so it lists them all,
//! whatever their labels: [`crate::forge_listing::list_open_issues_cached_all_as`],
//! the ETag-cached REST walk the intake pass and the stage-queue gauge
//! already read. An idle repo costs only free `304`s. The mirror backend
//! never lists: it reads the tick's worklist ([`crate::discovery`], #12307).

use std::path::Path;

use anyhow::Result;

use crate::forge_listing::RestIssue;

/// The `caller` the REST walk is accounted under in `loom.forge.calls`.
pub const CALLER: &str = "step_engine";

/// Every open issue and pull request of the repo at `cwd` (or `repo`),
/// oldest first.
///
/// # Errors
///
/// The REST walk's: a page failed or was shed, the walk passed its page cap,
/// or the listing moved mid-walk. The set is all-or-nothing, so a caller must
/// not read a missing item as absent.
pub fn list_all_open(
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo: Option<&str>,
) -> Result<Vec<RestIssue>> {
    // #12310: the step engine's items listing is discovery.
    let _discovery = crate::gh_invocation::purpose::discovery();
    crate::forge_listing::list_open_issues_cached_all_as(CALLER, gh_bin, cwd, repo)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "listing_tests.rs"]
mod tests;
