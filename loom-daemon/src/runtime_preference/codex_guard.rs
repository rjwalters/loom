//! Loom guard-hook readiness as a Codex availability condition for the
//! mutable and merging roles (#9390, #14497).
//!
//! # Why
//!
//! Champion merges and Judge issues the verdict Champion merges on. Both act
//! with forge authority that Loom's guards police (`guard-loom-workflow.sh`,
//! `guard-destructive.sh`): protected-branch pushes, direct merges outside
//! `merge-pr.sh`, label/verdict transitions. Claude enforces those guards
//! through its hooks on every run. On Codex they run only when the selected
//! profile carries Loom's managed `pre_tool_use` entry **and** Codex has trust
//! recorded for it where it runs. An untrusted entry is skipped without a
//! word, so the session gets no guard at all.
//!
//! So a merging role may run on Codex only when that is proven, and when it
//! is not, the ordered runtime preference must fall through to the next tap
//! (Claude) instead of selecting Codex and then failing the tick in
//! `spawn-codex.sh` (the "fallback never fires" half of #9390). This module
//! answers that availability question. `spawn-codex.sh` keeps its own
//! fail-closed check (`LOOM_CODEX_GUARDED_ROLES`), and a private-clone session
//! re-proves it in-container (`worker_setup::admit_role`).
//!
//! # The rule
//!
//! For a role that needs the hook, availability and account selection use the
//! same per-profile readiness check. Only ready shared profiles participate in
//! the draw. Other roles use the whole eligible pool.
//!
//! A session-managed profile whose registration is **sealed** (#10102,
//! [`codex_hooks::seal`]) is ready without a recorded trust decision:
//! `spawn-codex.sh` re-proves the seal for the actual launch (its cwd, its
//! argv, the container's own copies of the profile controls) and passes
//! `--dangerously-bypass-hook-trust`, or exits 78 if the proof fails there.
//!
//! Private-clone profiles are not judged here. Their registration is the
//! pinned, image-owned one, and a private launch re-proves the obligation
//! inside the container before the model runs.

use std::path::Path;

use crate::tokens_pool::codex_hooks::{self, Check, Registration};

/// Mirrors the union of mutable and merge/verdict roles in spawn-codex.sh.
/// A full sweep can perform the builder's work.
pub const GUARDED_ROLES: [&str; 5] = ["builder", "doctor", "champion", "judge", "sweep-lifecycle"];

#[must_use]
pub fn guarded(role: &str) -> bool {
    crate::runtime_admission::canonical_role(role).is_some_and(|role| GUARDED_ROLES.contains(&role))
}

/// Private clones prove the image-owned registration inside the container.
/// Shared profiles prove their own registration in this workspace.
pub(crate) fn ready(root: &Path, profile: &Path) -> bool {
    codex_hooks::is_private_clone(profile)
        || Check {
            codex_home: profile.to_owned(),
            workspace: Some(root.to_owned()),
            registration: Registration::WorkspaceIndependent,
            fallback_bridge: None,
            runtime_home: None,
            sealed: Some(codex_hooks::seal::Request {
                launch_dir: Some(root.to_owned()),
                ..Default::default()
            }),
        }
        .verify()
        .ready
}

/// The enabled shared profiles whose guard is not ready for this workspace.
pub fn unready_profiles(root: &Path) -> Result<Vec<String>, String> {
    use crate::tokens_pool::{account_inventory_quiet, AccountProvider};
    Ok(account_inventory_quiet(root, AccountProvider::Codex)
        .map_err(|error| format!("the Codex account inventory could not be read: {error}"))?
        .into_iter()
        .filter(|account| account.enabled && !ready(root, &account.credential_reference))
        .map(|account| account.id.name)
        .collect())
}

/// Restrict the draw before health and liveness ranking. A health fallback
/// can only widen to accounts which passed the same guard and repo checks.
pub(crate) fn select(
    root: &Path,
    model: Option<&str>,
    role: Option<&str>,
    keep: impl Fn(&crate::tokens_pool::AccountDescriptor) -> bool,
) -> anyhow::Result<crate::tokens_pool::account_registry::SelectedAccount> {
    let guarded_role = role.filter(|role| guarded(role));
    crate::tokens_pool::account_registry::select_codex_account_where(root, model, |account| {
        if !keep(account) {
            return false;
        }
        if let Some(role) = guarded_role.filter(|_| account.enabled) {
            if !ready(root, &account.credential_reference) {
                eprintln!(
                    "Skipping Codex account {} for {role}: guard hook is not ready",
                    account.id.name
                );
                return false;
            }
        }
        true
    })
}

#[cfg(test)]
#[path = "codex_guard_tests.rs"]
mod tests;
