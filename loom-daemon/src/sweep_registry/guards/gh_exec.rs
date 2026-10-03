//! The registry's one way to spawn `gh` (#10089).
//!
//! Every `sweep_registry` module builds its forge calls through these helpers
//! instead of a raw `Command::new(gh)`, so each call reaches
//! [`crate::forge_call_stats`] under a stable `op` name — and the rate-limit
//! breaker stops booking the registry's own spend as "external". The facade
//! applies what each site used to apply by hand: the cross-owner
//! `GH_CONFIG_DIR` for the workspace root (#5401) and `LOOM_REPO` as
//! `GH_REPO` (#8263). `gh issue|pr` sites keep their `--repo` flag too
//! ([`crate::claim_reconciliation::gh_call::loom_repo_flag`]).

use std::path::{Path, PathBuf};
use std::process::Output;

use crate::cmd_out::{CmdOutcome, Unavailable};
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
use crate::sweep_registry::reaper::reap_gh_timeout;
use crate::sweep_registry::SweepRegistry;

impl SweepRegistry {
    /// A facade invocation for this registry's workspace, bounded by
    /// [`reap_gh_timeout`].
    pub(in crate::sweep_registry) fn gh_inv(
        &self,
        op: &'static str,
        intent: AccessIntent,
        gh: &Path,
    ) -> GhInvocation {
        GhInvocation::new(Operation::new(op), intent, GhTarget::None, reap_gh_timeout())
            .program(gh)
            .current_dir(&self.config.workspace_root)
    }

    /// Run `inv`, shaped like
    /// [`output_with_timeout`](crate::sweep_registry::reaper::output_with_timeout):
    /// `Ok(Some(out))` when `gh` ran (any exit status), `Ok(None)` when it
    /// outlived the deadline (and was killed), `Err` when it could not be
    /// started or collected.
    pub(in crate::sweep_registry) fn run_counted(
        &self,
        inv: GhInvocation,
    ) -> std::io::Result<Option<Output>> {
        match inv.run() {
            CmdOutcome::Ran(out) => Ok(Some(out)),
            CmdOutcome::Unavailable(Unavailable::TimedOut { .. }) => Ok(None),
            CmdOutcome::Unavailable(u) => Err(std::io::Error::other(u.to_string())),
        }
    }

    /// The configured `gh` binary, or the crate's single resolver
    /// ([`crate::gh_invocation::gh_bin`]: policy launcher -> `LOOM_GH_BIN` ->
    /// `PATH`) when no override is configured (#9985).
    pub(in crate::sweep_registry) fn resolved_gh(&self) -> PathBuf {
        self.config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from(crate::gh_invocation::gh_bin()))
    }

    /// The configured `gh` binary, else bare `gh` — which the facade resolves
    /// through its own ladder (so the span records the real source).
    fn configured_gh(&self) -> PathBuf {
        self.config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"))
    }

    /// [`Self::gh_inv`] + [`Self::run_counted`] for a read.
    pub(in crate::sweep_registry) fn gh_read<I, S>(
        &self,
        op: &'static str,
        args: I,
    ) -> std::io::Result<Option<Output>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let gh = self.configured_gh();
        self.run_counted(self.gh_inv(op, AccessIntent::Read, &gh).args(args))
    }

    /// [`Self::gh_read`] for a write.
    pub(in crate::sweep_registry) fn gh_write<I, S>(
        &self,
        op: &'static str,
        args: I,
    ) -> std::io::Result<Option<Output>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let gh = self.configured_gh();
        self.run_counted(self.gh_inv(op, AccessIntent::Write, &gh).args(args))
    }
}
