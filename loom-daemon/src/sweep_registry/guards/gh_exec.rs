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
    /// [`reap_gh_timeout`]. A read is [`GhInvocation::reader_only`] (#12555).
    pub(in crate::sweep_registry) fn gh_inv(
        &self,
        op: &'static str,
        intent: AccessIntent,
        gh: &Path,
    ) -> GhInvocation {
        let inv = GhInvocation::new(Operation::new(op), intent, GhTarget::None, reap_gh_timeout())
            .program(gh)
            .current_dir(&self.config.workspace_root);
        // #12555: a registry read is a reader's or the gateway's, or it is
        // shed — never the writer's (#12394).
        let inv = if intent == AccessIntent::Read {
            inv.reader_only()
        } else {
            inv
        };
        match crate::claim_reconciliation::gh_call::forge_op_for(op) {
            Some(fop) => inv.forge_op(fop),
            None => inv,
        }
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

    /// The workspace's repo-facts answer under `gh repo view` semantics (W3a),
    /// read with this registry's own `gh`.
    pub(in crate::sweep_registry) fn owner_repo_fact(&self) -> crate::forge_repo_facts::Lookup {
        crate::forge_repo_facts::canonical_with(
            &self.configured_gh(),
            &self.config.workspace_root,
            crate::forge_repo_facts::GhRepoEnv::Ignore,
        )
    }

    /// The configured `gh` binary, else bare `gh` — which the facade resolves
    /// through its own ladder (so the span records the real source).
    fn configured_gh(&self) -> PathBuf {
        self.config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"))
    }

    /// [`Self::gh_inv`] + [`Self::run_counted`] for a read: reader-only
    /// (#12555), including a read-back of this daemon's own write — see
    /// [`crate::claim_reconciliation::gh_call::read`] for why read-your-write
    /// does not need the writer.
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sweep_registry::SweepRegistryConfig;

    /// #12555: every registry read is reader-only — it can never mint the
    /// writer, whatever the site adds — and a write is not. #12571: asserts
    /// the resolved credential too: a read forced through `writer_identity()`
    /// still derives a reader route, a write derives none.
    #[test]
    fn registry_reads_are_reader_only_and_writes_are_not() {
        use crate::gh_invocation::cwd_route::{derive_with, CwdAnswer, Derivation, DeriveEnv};
        let dir = tempfile::tempdir().unwrap();
        let reg = SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf()));
        let gh = Path::new("gh");
        let comments = ["api", "repos/{owner}/{repo}/issues/7/comments"];
        let derived = |inv: GhInvocation| {
            let inv = inv.current_dir(dir.path()).args(comments);
            derive_with(&inv, &DeriveEnv::default(), &|_: &Path, _| {
                CwdAnswer::Sole("acme/widget".to_string())
            })
        };
        let routes_to_reader = |d: Derivation| matches!(d, Derivation::Route { .. });
        assert!(reg
            .gh_inv("guard.lease_comments", AccessIntent::Read, gh)
            .is_reader_only());
        let pinned = reg
            .gh_inv("guard.claim_timeline", AccessIntent::Read, gh)
            .writer_identity();
        assert!(pinned.is_reader_only());
        assert!(
            routes_to_reader(derived(pinned)),
            "writer_identity() must not pin a registry read to the writer credential"
        );
        let write = reg.gh_inv("guard.flip_building", AccessIntent::Write, gh);
        assert!(!write.is_reader_only());
        assert!(!routes_to_reader(derived(write)), "a write must never derive a reader route");
    }
}
