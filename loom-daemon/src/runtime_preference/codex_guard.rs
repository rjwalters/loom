//! Loom guard-hook readiness as a Codex availability condition for the
//! merging roles (rjwalters/loom#9390).
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
//! Every enabled, shared (host-mounted) Codex profile must be `ready` by
//! [`codex_hooks::Check`], the same readiness `provision-codex-hooks.sh
//! verify` reports, for this workspace: the workspace-independent
//! registration, a readable bridge in this checkout, the pinned receipt, and
//! trust recorded at Loom's key in the runtime location. Requiring *every*
//! profile, not *some*, keeps selection simple: whichever profile the pool
//! then draws is ready, so no draw can land on an unguarded seat. Once an
//! operator readies the last profile, the gate opens by itself on the next
//! tick, and it closes again if one later goes stale.
//!
//! Private-clone profiles are not judged here. Their registration is the
//! pinned, image-owned one, and a private launch re-proves the obligation
//! inside the container before the model runs.

use std::path::Path;

use crate::tokens_pool::codex_hooks::{self, Check, Registration};

/// Roles that act with merge or verdict authority on the forge and therefore
/// need Loom's guards enforced on every runtime. They do not write the local
/// repository, so they are not `spawn-codex.sh`'s mutable roles: they keep
/// the read-only sandbox. Mirrors `LOOM_CODEX_GUARDED_ROLES` in
/// `spawn-codex.sh`.
pub const GUARDED_ROLES: [&str; 2] = ["champion", "judge"];

/// Whether `role` (any spelling `canonical_role` accepts) is a merging role
/// that needs Loom's guard hook proven on Codex.
#[must_use]
pub fn guarded(role: &str) -> bool {
    crate::runtime_admission::canonical_role(role).is_some_and(|role| GUARDED_ROLES.contains(&role))
}

/// The enabled shared Codex profiles whose Loom guard hook is NOT ready for
/// this workspace, by account name. `Ok(vec![])` means every one is ready.
///
/// # Errors
/// A secret-free reason when the account inventory cannot be read. A caller
/// must treat that as "not ready".
pub fn unready_profiles(root: &Path) -> Result<Vec<String>, String> {
    use crate::tokens_pool::{account_inventory_quiet, AccountProvider};
    let accounts = account_inventory_quiet(root, AccountProvider::Codex)
        .map_err(|error| format!("the Codex account inventory could not be read: {error}"))?;
    Ok(accounts
        .into_iter()
        .filter(|account| account.enabled)
        .filter(|account| !codex_hooks::is_private_clone(&account.credential_reference))
        .filter(|account| {
            !Check {
                codex_home: account.credential_reference.clone(),
                workspace: Some(root.to_path_buf()),
                registration: Registration::WorkspaceIndependent,
                fallback_bridge: None,
                runtime_home: None,
            }
            .verify()
            .ready
        })
        .map(|account| account.id.name)
        .collect())
}

#[cfg(test)]
#[path = "codex_guard_tests.rs"]
mod tests;
