//! The no-daemon read (#8876): the existing ambient `GhInvocation` path, but
//! behind the same forge-egress admission gate the daemon applies.
//!
//! On an unconfigured host the gate admits silently and nothing changes. On a
//! governed host the facade already prefers the policy's managed launcher; a
//! policy that refuses forge work yields `Unknown`, never a direct request.

use std::path::Path;

use super::{parse_state, ReadOutcome};
use crate::forge_egress::policy::PolicySources;

/// One standalone read and the route it actually took.
pub struct StandaloneRead {
    pub outcome: ReadOutcome,
    pub route: String,
}

/// Read `issue`'s state from `repo_root` under the effective local routing.
#[must_use]
pub fn read(repo_root: &Path, sources: &PolicySources, issue: u64) -> StandaloneRead {
    if let Some(refusal) = crate::forge_egress::gate::dispatch_refusal_with(sources, repo_root) {
        return StandaloneRead {
            outcome: ReadOutcome::Unknown,
            route: format!("standalone-refused:{}", refusal.codes.join(",")),
        };
    }
    let source = crate::gh_invocation::resolver::resolve().source.as_str();
    let outcome = crate::gh_state_probe::issue_state_output(repo_root, issue)
        .filter(|o| o.status.success())
        .map_or(ReadOutcome::Unknown, |o| parse_state(&o.stdout));
    StandaloneRead {
        outcome,
        route: format!("standalone-gh/{source}"),
    }
}
