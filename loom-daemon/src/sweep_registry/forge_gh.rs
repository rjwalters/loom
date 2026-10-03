//! The registry's one way to build a `gh` invocation (#9985 slice 5).
//!
//! Every forge probe and mutation in `sweep_registry` used to assemble its own
//! `Command`: the configured `gh` (or a bare `"gh"` that ignored
//! `LOOM_GH_BIN`), the workspace as `current_dir` (#3937), the #5401
//! cross-owner `GH_CONFIG_DIR`, the `LOOM_REPO` → `GH_REPO` override (#8263)
//! and the reaper bound (#3973). [`SweepRegistry::gh`] carries all of that
//! into [`crate::gh_invocation::GhInvocation`], which owns execution.

use super::reaper::reap_gh_timeout;
use super::SweepRegistry;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

impl SweepRegistry {
    /// A `gh` invocation scoped to this registry:
    ///
    /// - runs in `workspace_root`, so a bare issue number resolves against
    ///   *this* repo in a multi-workspace daemon (#3928/#3937), and the
    ///   facade applies that root's cross-owner `GH_CONFIG_DIR` (#5401);
    /// - runs the configured `SweepRegistryConfig.gh_bin` when set (test
    ///   harnesses), else the crate's single resolver — never a bare `PATH`
    ///   lookup that skips `LOOM_GH_BIN`;
    /// - is bounded by [`reap_gh_timeout`] (#3973).
    ///
    /// Run it with [`GhInvocation::output_bounded`] for the
    /// `output_with_timeout` result shape these sites already match on.
    pub(crate) fn gh(
        &self,
        operation: &'static str,
        intent: AccessIntent,
        target: GhTarget,
    ) -> GhInvocation {
        let inv = GhInvocation::new(Operation::new(operation), intent, target, reap_gh_timeout())
            .current_dir(&self.config.workspace_root);
        match self.config.gh_bin.as_deref() {
            Some(gh_bin) => inv.program(gh_bin),
            None => inv,
        }
    }

    /// [`gh`](Self::gh) typed to `owner/repo` (a `gh api repos/{owner}/{repo}/…`
    /// call that already resolved its slug).
    pub(crate) fn gh_for_repo(
        &self,
        operation: &'static str,
        intent: AccessIntent,
        owner: &str,
        repo: &str,
    ) -> GhInvocation {
        let target = GhTarget::Repo {
            owner: owner.to_string(),
            repo: repo.to_string(),
        };
        self.gh(operation, intent, target)
    }
}

/// `["--repo", $LOOM_REPO]` when the machine-global override is set, else
/// nothing — the trailing flag the registry's `gh issue …` calls have always
/// appended (never for `gh api`, which has no `--repo`, #8263).
pub(crate) fn loom_repo_flag() -> Vec<String> {
    std::env::var("LOOM_REPO")
        .map(|repo| vec!["--repo".to_string(), repo])
        .unwrap_or_default()
}
