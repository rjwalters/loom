//! Loom guard-hook readiness as a Codex availability condition for the
//! merging roles (rjwalters/loom#9390 follow-up).
//!
//! # Why
//!
//! Champion merges and Judge issues the verdict Champion merges on. Both act
//! with forge authority that Loom's guards police (`guard-loom-workflow.sh`,
//! `guard-destructive.sh`): protected-branch pushes, direct `gh pr merge`
//! outside `merge-pr.sh`, label/verdict transitions. Claude enforces those
//! guards through its hooks on every run. On Codex they run only when the
//! selected profile carries Loom's managed `pre_tool_use` entry **and** Codex
//! has trust recorded for it at the location it runs from. An untrusted entry
//! is skipped without a word, so the session gets no guard at all.
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
//! Every enabled, shared (host-mounted) Codex profile must verify `ready`
//! under `provision-codex-hooks.sh verify` for this workspace. That script is
//! the one readiness implementation, used here unchanged: the
//! workspace-independent registration, a readable bridge in this checkout,
//! the pinned receipt, and trust recorded at Loom's key in the runtime
//! location. Requiring *every* profile, not *some*, keeps selection simple:
//! whichever profile the pool then draws is ready, so no draw can land on an
//! unguarded seat. Once an operator readies the last profile, the gate opens
//! by itself on the next tick, and it closes again if one later goes stale.
//!
//! Private-clone profiles are not judged here. Their registration is the
//! pinned, image-owned one, which a host-side `verify` cannot evaluate. A
//! private launch re-proves the obligation inside the container before the
//! model runs, and refuses if it does not hold.
//!
//! # Cost
//!
//! One `bash provision-codex-hooks.sh verify` per shared profile, only for a
//! Codex tap of a merging role whose credential pool is the wall. On a
//! seven-seat host that is a few hundred milliseconds per Champion or Judge
//! tick, on the decision path that already reads account inventory and
//! health files.

use std::path::{Path, PathBuf};
use std::process::Command;

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
/// A secret-free reason when readiness cannot be evaluated at all: the
/// account inventory is unreadable, or no `provision-codex-hooks.sh` is
/// installed. A caller must treat that as "not ready".
pub fn unready_profiles(root: &Path) -> Result<Vec<String>, String> {
    use crate::tokens_pool::{account_inventory_quiet, AccountProvider};
    let accounts = account_inventory_quiet(root, AccountProvider::Codex)
        .map_err(|error| format!("the Codex account inventory could not be read: {error}"))?;
    let provisioner = provisioner(root).ok_or_else(|| {
        "no provision-codex-hooks.sh is installed for this workspace, so Loom's guard hook \
         cannot be verified"
            .to_string()
    })?;
    Ok(accounts
        .into_iter()
        .filter(|account| account.enabled)
        .filter(|account| !private_clone(&account.credential_reference))
        .filter(|account| !profile_ready(&provisioner, root, &account.credential_reference))
        .map(|account| account.id.name)
        .collect())
}

fn provisioner(root: &Path) -> Option<PathBuf> {
    let (_, _, scripts) = crate::runtime_admission::roots(root);
    let path = scripts.join("provision-codex-hooks.sh");
    path.is_file().then_some(path)
}

/// `<profile root>/.private-sessions/<name>/workspace.json`: the state
/// `private_workspace::lifecycle::start` writes for a private-clone session.
fn private_clone(profile: &Path) -> bool {
    let (Some(parent), Some(name)) = (profile.parent(), profile.file_name()) else {
        return false;
    };
    parent
        .join(".private-sessions")
        .join(name)
        .join("workspace.json")
        .is_file()
}

fn profile_ready(provisioner: &Path, root: &Path, profile: &Path) -> bool {
    Command::new("bash")
        .arg(provisioner)
        .arg("verify")
        .arg("--codex-home")
        .arg(profile)
        .arg("--workspace")
        .arg(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
#[path = "codex_guard_tests.rs"]
mod tests;
