//! Which account registry a `loom-daemon accounts` verb acts on (issue #8540).
//!
//! # The problem this module exists to remove
//!
//! `accounts` takes `--workspace`, defaulted to `.`, and every registry read
//! and write is `<workspace>/.loom/accounts.json`. That made the *target of a
//! mutation* an implicit function of where the operator happened to be
//! standing, with two consequences observed in one session (#8540):
//!
//! 1. From `$HOME` every verb failed with `Codex profile root must not be
//!    repository-local`. The profile root (`~/.loom/codex-profiles`) had not
//!    moved; the "workspace" had become `$HOME`, which contains it by
//!    construction. The message named the thing that was fine.
//! 2. `accounts disable codex <name>` from a repo checkout wrote the
//!    repo-local registry only — the shared registry still had the account
//!    enabled — and no output said which file had been written.
//!
//! # The rule
//!
//! - An explicit `--workspace <path>` is honoured literally. The operator
//!   named a workspace; redirecting it elsewhere would be the same class of
//!   surprise this module removes.
//! - Otherwise (the `.` default, i.e. "wherever I am") the nearest enclosing
//!   Loom workspace — the first ancestor holding a `.loom/` directory — wins,
//!   so a verb run from `<repo>/loom-daemon/src` acts on `<repo>`'s registry
//!   rather than inventing one in a source directory.
//! - A cwd inside no Loom workspace at all resolves to the **shared**
//!   machine-level registry ([`shared_accounts_root`]) instead of erroring, and
//!   only errors — naming `--workspace` — when the shared registry is
//!   explicitly disabled.
//!
//! [`AccountsRegistry::describe`] is what every verb prints, so "which
//! registry did that act on" is answered in the output rather than inferred.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use super::account_registry::codex_registry_names;
use super::paths::{
    is_shared_accounts_root, per_repo_accounts_file, shared_accounts_root, SHARED_ACCOUNTS_ROOT_ENV,
};

/// Which of the two registries an `accounts` invocation resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryScope {
    /// A workspace-local `<workspace>/.loom/accounts.json`.
    Repo,
    /// The machine-level registry shared by every workspace on this host.
    Shared,
}

impl RegistryScope {
    /// The word used in operator output. Deliberately the vocabulary the issue
    /// asked for (`repo: <path>` / `shared: <path>`) rather than the
    /// `InventoryProvenance` Debug spelling, which is capitalised and appears
    /// per-row.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::Shared => "shared",
        }
    }
}

/// The registry an `accounts` invocation acts on, resolved once up front.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountsRegistry {
    /// The workspace root whose `.loom/accounts.json` is the registry — also
    /// the workspace handed to the lifecycle service, so the profile-root
    /// check compares against *this*, never against a `$HOME`-shaped cwd.
    pub workspace: PathBuf,
    pub scope: RegistryScope,
}

impl AccountsRegistry {
    #[must_use]
    pub fn registry_path(&self) -> PathBuf {
        per_repo_accounts_file(&self.workspace)
    }

    /// `repo: /path/to/repo/.loom/accounts.json` — the single line every verb
    /// prints so the acted-on registry is never implicit.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("{}: {}", self.scope.label(), self.registry_path().display())
    }

    #[must_use]
    pub const fn is_shared(&self) -> bool {
        matches!(self.scope, RegistryScope::Shared)
    }
}

/// Resolve the registry for an already-absolute `workspace`.
///
/// `explicit` is whether the operator actually passed `--workspace`; see the
/// module docs for why that distinction is load-bearing (an explicitly named
/// workspace must never be silently redirected to the shared registry — that
/// would let a verb aimed at a scratch directory write the operator's live
/// machine-level registry instead).
pub fn resolve_accounts_registry(workspace: &Path, explicit: bool) -> Result<AccountsRegistry> {
    if explicit {
        return Ok(classify(workspace.to_path_buf()));
    }
    if let Some(enclosing) = enclosing_loom_workspace(workspace) {
        return Ok(classify(enclosing));
    }
    let shared = shared_accounts_root().ok_or_else(|| {
        anyhow!(
            "{} is not inside a Loom workspace (no `.loom/` directory here or in any parent \
             directory), and the shared machine-level account registry is disabled \
             ({SHARED_ACCOUNTS_ROOT_ENV} is set to an empty value). Pass `--workspace <path>` to \
             name the workspace whose `.loom/accounts.json` you mean, or set \
             {SHARED_ACCOUNTS_ROOT_ENV} to the directory that holds the shared registry.",
            workspace.display()
        )
    })?;
    Ok(AccountsRegistry {
        workspace: shared,
        scope: RegistryScope::Shared,
    })
}

/// A workspace is "the shared registry" exactly when it *is* the shared root —
/// the same `~/.loom/accounts.json` that a `--workspace .` from `$HOME` has
/// always resolved to. Everything else is repo-local.
fn classify(workspace: PathBuf) -> AccountsRegistry {
    let scope = if is_shared_accounts_root(&workspace) {
        RegistryScope::Shared
    } else {
        RegistryScope::Repo
    };
    AccountsRegistry { workspace, scope }
}

/// The nearest ancestor of `start` (including `start` itself) that holds a
/// `.loom/` directory, i.e. the Loom workspace the operator is standing in.
#[must_use]
pub fn enclosing_loom_workspace(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|candidate| candidate.join(".loom").is_dir())
        .map(Path::to_path_buf)
}

/// Codex account names this repo-local registry shadows in the shared one
/// (issue #8540, mirroring the API-key pool's `add`-time shadow note).
///
/// Shadowing here is the operator-facing fact, not a precedence mechanism:
/// both files hold an entry for the same name, the repo-local one is what this
/// invocation reads and writes, and a `disable` against it leaves the shared
/// entry enabled — exactly the surprise reported in #8540.
///
/// Best-effort and secret-free: a registry that cannot be read contributes no
/// names (the verb itself fails loudly on an unreadable registry it needs).
/// Always empty when already acting on the shared registry — a registry cannot
/// shadow itself.
#[must_use]
pub fn shadowed_shared_accounts(registry: &AccountsRegistry) -> Vec<String> {
    if registry.is_shared() {
        return Vec::new();
    }
    let Some(shared_root) = shared_accounts_root() else {
        return Vec::new();
    };
    if shared_root == registry.workspace {
        return Vec::new();
    }
    let Ok(shared_names) = codex_registry_names(&shared_root) else {
        return Vec::new();
    };
    let Ok(local_names) = codex_registry_names(&registry.workspace) else {
        return Vec::new();
    };
    let mut shadowed: Vec<String> = local_names
        .into_iter()
        .filter(|name| shared_names.contains(name))
        .collect();
    shadowed.sort();
    shadowed.dedup();
    shadowed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn write_registry(workspace: &Path, names: &[&str]) {
        let path = per_repo_accounts_file(workspace);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let accounts: Vec<String> = names
            .iter()
            .map(|name| {
                format!(
                    "{{\"provider\":\"codex\",\"name\":\"{name}\",\
                     \"credential_kind\":\"codex_home\",\"credential_reference\":\"{name}\",\
                     \"enabled\":true}}"
                )
            })
            .collect();
        std::fs::write(&path, format!("{{\"version\":1,\"accounts\":[{}]}}", accounts.join(",")))
            .unwrap();
    }

    /// Issue #8540 acceptance criterion 1: a cwd inside no Loom workspace
    /// resolves to the shared machine-level registry rather than erroring (and
    /// rather than inventing `<cwd>/.loom/accounts.json`).
    #[test]
    #[serial]
    fn a_non_workspace_cwd_resolves_to_the_shared_registry() {
        let shared = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::env::set_var(SHARED_ACCOUNTS_ROOT_ENV, shared.path());

        let resolved = resolve_accounts_registry(elsewhere.path(), false).unwrap();
        assert_eq!(resolved.scope, RegistryScope::Shared);
        assert_eq!(resolved.workspace, shared.path());
        assert_eq!(resolved.registry_path(), per_repo_accounts_file(shared.path()));
        assert!(resolved.describe().starts_with("shared: "), "{}", resolved.describe());

        std::env::remove_var(SHARED_ACCOUNTS_ROOT_ENV);
    }

    /// The nearest enclosing workspace wins, so a verb run from a source
    /// subdirectory acts on the repo's registry instead of creating one there.
    #[test]
    #[serial]
    fn a_cwd_below_a_workspace_resolves_to_that_workspace() {
        let shared = tempfile::tempdir().unwrap();
        std::env::set_var(SHARED_ACCOUNTS_ROOT_ENV, shared.path());
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".loom")).unwrap();
        let nested = repo.path().join("loom-daemon").join("src");
        std::fs::create_dir_all(&nested).unwrap();

        let resolved = resolve_accounts_registry(&nested, false).unwrap();
        assert_eq!(resolved.scope, RegistryScope::Repo);
        // `tempfile` hands back the uncanonicalized path on macOS
        // (`/var/...` vs `/private/var/...`); compare on the shape that
        // matters, which is "the repo root, not the nested directory".
        assert_eq!(resolved.workspace, repo.path());
        assert!(resolved.describe().starts_with("repo: "), "{}", resolved.describe());

        std::env::remove_var(SHARED_ACCOUNTS_ROOT_ENV);
    }

    /// An explicit `--workspace` is never redirected: the operator named it.
    #[test]
    #[serial]
    fn an_explicit_workspace_is_honoured_even_without_a_loom_directory() {
        let shared = tempfile::tempdir().unwrap();
        std::env::set_var(SHARED_ACCOUNTS_ROOT_ENV, shared.path());
        let scratch = tempfile::tempdir().unwrap();

        let resolved = resolve_accounts_registry(scratch.path(), true).unwrap();
        assert_eq!(resolved.scope, RegistryScope::Repo);
        assert_eq!(resolved.workspace, scratch.path());

        // ...and naming the shared root explicitly reaches the shared registry,
        // which is how a repo-shadowed shared account stays manageable.
        let resolved = resolve_accounts_registry(shared.path(), true).unwrap();
        assert_eq!(resolved.scope, RegistryScope::Shared);

        std::env::remove_var(SHARED_ACCOUNTS_ROOT_ENV);
    }

    /// With the shared registry explicitly disabled there is nothing to fall
    /// back to, so the error must name the way out rather than the profile
    /// root (the #8540 misdirection).
    #[test]
    #[serial]
    fn a_disabled_shared_registry_errors_naming_workspace() {
        std::env::set_var(SHARED_ACCOUNTS_ROOT_ENV, "");
        let elsewhere = tempfile::tempdir().unwrap();

        let error = resolve_accounts_registry(elsewhere.path(), false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--workspace"), "{error}");
        assert!(error.contains("not inside a Loom workspace"), "{error}");
        assert!(!error.contains("repository-local"), "{error}");

        std::env::remove_var(SHARED_ACCOUNTS_ROOT_ENV);
    }

    /// Issue #8540 acceptance criterion 3: same name in both registries is
    /// reported, so a `disable` that leaves the shared entry enabled is not a
    /// silent surprise.
    #[test]
    #[serial]
    fn same_named_accounts_in_both_registries_are_reported_as_shadowed() {
        let shared = tempfile::tempdir().unwrap();
        std::env::set_var(SHARED_ACCOUNTS_ROOT_ENV, shared.path());
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".loom")).unwrap();
        write_registry(shared.path(), &["alpha", "beta"]);
        write_registry(repo.path(), &["alpha", "gamma"]);

        let resolved = resolve_accounts_registry(repo.path(), false).unwrap();
        assert_eq!(shadowed_shared_accounts(&resolved), vec!["alpha".to_string()]);

        // Acting on the shared registry itself shadows nothing.
        let shared_registry = resolve_accounts_registry(shared.path(), true).unwrap();
        assert!(shadowed_shared_accounts(&shared_registry).is_empty());

        std::env::remove_var(SHARED_ACCOUNTS_ROOT_ENV);
    }
}
