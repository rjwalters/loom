//! The no-daemon read (#8876): the existing ambient `GhInvocation` path.

use std::path::Path;

use super::{parse_state, ReadOutcome};

/// One standalone read and the route it actually took.
pub struct StandaloneRead {
    pub outcome: ReadOutcome,
    pub route: String,
}

/// Read `issue`'s state from `repo_root` under the effective local routing.
#[must_use]
pub fn read(repo_root: &Path, issue: u64) -> StandaloneRead {
    let source = crate::gh_invocation::resolver::resolve().source.as_str();
    let outcome = crate::gh_state_probe::issue_state_output(repo_root, issue)
        .filter(|o| o.status.success())
        .map_or(ReadOutcome::Unknown, |o| parse_state(&o.stdout));
    StandaloneRead {
        outcome,
        route: format!("standalone-gh/{source}"),
    }
}
