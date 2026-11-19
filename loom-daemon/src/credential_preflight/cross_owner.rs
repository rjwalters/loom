//! Per-owner (cross-owner) credential establishment (#5401), and retrying it
//! until it succeeds (#11957).
//!
//! The GitHub App preflight mints ONE installation token, for the daemon's
//! workspace-root owner. Every managed repo owned by a different account/org
//! needs its own installation token, published into its own per-owner
//! `GH_CONFIG_DIR` and registered against each of that owner's checkout roots
//! ([`super::register_root_gh_config_dir`]), or it is probed and written
//! under the root owner's credential and refused.
//!
//! Before #11957 this ran exactly once, at startup, and only when the
//! startup mint succeeded. A startup mint that timed out on a saturated host
//! left the registry empty for the life of the process: the primary refresh
//! tick recovered the root owner's token five minutes later but never ran
//! this, so every cross-owner root fell through to the process credential
//! and the write-scope gate refused it as "not in this App installation".
//!
//! Now [`establish_for_workspace`] is the one entry point, called by startup
//! and by the primary refresh tick, and [`CrossOwnerRetry`] decides when the
//! tick runs it: on the first successful tick when startup never ran it, and
//! then with bounded backoff while any owner's mint still fails.
//!
//! A pass also expires what the write-scope gate cached for the credential a
//! newly registered root was probed under before
//! ([`invalidate_write_scope`]). Those refusals were definitive answers about
//! the wrong credential, cached for the probe TTL; without the expiry a
//! `may-write` asked under that credential keeps serving them after the fix.

use std::path::{Path, PathBuf};

use super::{
    gh_config_dir_for_owner_slug, gh_config_dir_for_root, github_app_gh_config_dir_for_owner,
    nwo_from_git_remote, owner_of_nwo, plan_cross_owner_credentials, publish_outcome,
    register_owner_gh_config_dir, register_owner_refresh_source, register_root_gh_config_dir,
    CrossOwnerCredentialPlan, GithubAppMinter, GithubAppOutcome, RealGithubAppMinter,
};

/// What one establishment pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrossOwnerPass {
    /// Owners whose credential is registered after this pass (newly, or
    /// already by an earlier pass or the #6171 forced refresh).
    pub established: Vec<String>,
    /// Owners whose mint or publish failed this pass. Non-empty means the
    /// pass should be retried.
    pub failed: Vec<String>,
}

impl CrossOwnerPass {
    /// Every planned owner is registered (or there was nothing to plan).
    #[must_use]
    pub fn complete(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Every effective managed root of `workspace_root`'s registry, paired with
/// the `owner/repo` its git remote names. Roots with no GitHub remote are
/// dropped.
///
/// The `bool` is whether the registry was read: `false` means the read failed
/// and the list is only the workspace-root fallback, so it is not
/// authoritative about which owners are still managed (#14450).
#[must_use]
pub fn managed_roots(workspace_root: &Path) -> (Vec<(PathBuf, String)>, bool) {
    let (registry, loaded) = match crate::workspace_registry::WorkspaceRegistry::load_default() {
        Ok(registry) => (registry, true),
        Err(e) => {
            log::warn!(
                "credential_preflight: workspace registry unreadable ({e:#}); not reaping \
                 per-owner credentials for owners with no managed repo — #14450"
            );
            (Default::default(), false)
        }
    };
    let managed = registry
        .effective_roots(workspace_root)
        .into_iter()
        .filter_map(|root| nwo_from_git_remote(&root).map(|nwo| (root, nwo)))
        .collect();
    (managed, loaded)
}

/// [`establish_for_workspace`] on a blocking thread, for the refresh tick's
/// #11957 retry; a join error is reported as a failed pass so it retries.
pub async fn establish_for_workspace_off_thread(
    workspace_root: PathBuf,
    root_owner_repo: String,
    script_path: PathBuf,
) -> CrossOwnerPass {
    tokio::task::spawn_blocking(move || {
        establish_for_workspace(&workspace_root, &root_owner_repo, &script_path)
    })
    .await
    .unwrap_or_else(|e| CrossOwnerPass {
        failed: vec![format!("per-owner pass join error: {e}")],
        ..Default::default()
    })
}

/// [`establish_cross_owner_credentials`] for the daemon's real workspace:
/// its registry's roots, minted through `github-app-token.sh` at
/// `script_path`. Blocking (git remote reads and mint subprocesses).
#[must_use]
pub fn establish_for_workspace(
    workspace_root: &Path,
    root_owner_repo: &str,
    script_path: &Path,
) -> CrossOwnerPass {
    let (managed, registry_loaded) = managed_roots(workspace_root);
    let minter = RealGithubAppMinter {
        script_path: script_path.to_path_buf(),
        cwd: workspace_root.to_path_buf(),
    };
    establish_cross_owner_credentials_with(
        workspace_root,
        owner_of_nwo(root_owner_repo),
        &managed,
        registry_loaded,
        &minter,
    )
}

/// Mint, publish and register a per-owner credential for every owner in
/// `managed` other than `root_owner` (#5401).
///
/// An owner that already has a registered credential (an earlier pass, or
/// #6171's forced refresh after a scan failure) is not re-minted; its roots
/// are registered against the existing dir, which covers the roots the
/// forced refresh never saw. Every root whose registration changes has the
/// write-scope answers cached under its previous credential expired.
pub fn establish_cross_owner_credentials(
    workspace_root: &Path,
    root_owner: &str,
    managed: &[(PathBuf, String)],
    minter: &dyn GithubAppMinter,
) -> CrossOwnerPass {
    establish_cross_owner_credentials_with(workspace_root, root_owner, managed, true, minter)
}

/// [`establish_cross_owner_credentials`] with `managed_authoritative` false
/// when `managed` came from a failed registry read (#14450): the reap then
/// skips its no-plan arm, since `managed` is only the root fallback.
pub fn establish_cross_owner_credentials_with(
    workspace_root: &Path,
    root_owner: &str,
    managed: &[(PathBuf, String)],
    managed_authoritative: bool,
    minter: &dyn GithubAppMinter,
) -> CrossOwnerPass {
    let mut pass = CrossOwnerPass::default();
    let plans = plan_cross_owner_credentials(root_owner, managed);
    reap_stale_owner_credentials(
        workspace_root,
        root_owner,
        managed,
        managed_authoritative,
        &plans,
    );
    for plan in plans {
        let owner_dir = match gh_config_dir_for_owner_slug(&plan.representative_owner_repo) {
            Some(dir) => dir,
            None => match mint_and_publish(workspace_root, &plan, minter) {
                Ok(Some(dir)) => dir,
                // The App is not configured after all: nothing to establish,
                // and nothing a retry would change.
                Ok(None) => continue,
                Err(()) => {
                    pass.failed.push(plan.owner.clone());
                    continue;
                }
            },
        };
        register_plan(&plan, &owner_dir);
        pass.established.push(plan.owner);
    }
    pass
}

/// The writer-credential files `publish_minted` leaves in a by-owner
/// directory. A reader App's tokens live in `<owner>/<app id>/` subdirectories
/// and are never in this list.
const OWNER_WRITER_FILES: [&str; 4] = ["hosts.yml", "hosts.yml.tmp", "config.yml", "identity.json"];

/// Remove the writer credential of every by-owner directory whose owner is
/// no longer a cross-owner (#14450), so a leftover directory (for example
/// the old root owner's after the root checkout changed owner) cannot be
/// preferred over the live credential. Run on every establishment pass, over
/// the primary root and every managed root:
///
/// - an owner equal to `root_owner` is stale in every root (the root owner's
///   repos ride the process-global credential, never a by-owner directory);
/// - in the primary root, an owner with no plan (no managed repo left) is
///   stale too, but only when `managed_authoritative` (the registry was read)
///   and `managed` is non-empty: a failed read must not wipe live credentials.
///
/// Only the writer files are removed, then the directory itself if that left
/// it empty: a reader App's `<owner>/<app id>/` subdirectories, the live
/// root-owner credential (`.loom/gh-config`) and every other cross-owner's
/// directory are never touched. Failures are logged and skipped.
fn reap_stale_owner_credentials(
    workspace_root: &Path,
    root_owner: &str,
    managed: &[(PathBuf, String)],
    managed_authoritative: bool,
    plans: &[CrossOwnerCredentialPlan],
) {
    let mut roots: Vec<&Path> = vec![workspace_root];
    for (root, _) in managed {
        if !roots.contains(&root.as_path()) {
            roots.push(root.as_path());
        }
    }
    for root in roots {
        let is_primary = root == workspace_root;
        let by_owner = root.join(".loom").join("gh-config-by-owner");
        let Ok(entries) = std::fs::read_dir(&by_owner) else {
            continue;
        };
        for entry in entries.flatten() {
            // `file_type` does not follow a symlink: never reap through one.
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            let Some(owner) = name
                .to_str()
                .filter(|n| crate::forge_bucket_book::valid_owner(n))
            else {
                continue;
            };
            let stale = (!root_owner.is_empty() && owner.eq_ignore_ascii_case(root_owner))
                || (is_primary
                    && managed_authoritative
                    && !managed.is_empty()
                    && !plans.iter().any(|plan| plan.owner == owner));
            if stale {
                remove_owner_writer_credential(&entry.path(), owner);
            }
        }
    }
}

/// Delete `dir`'s writer files, then `dir` if empty (see
/// [`reap_stale_owner_credentials`]).
fn remove_owner_writer_credential(dir: &Path, owner: &str) {
    let mut removed = false;
    for file in OWNER_WRITER_FILES {
        match std::fs::remove_file(dir.join(file)) {
            Ok(()) => removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!(
                "credential_preflight: could not remove stale per-owner credential file {} \
                 ({e}); continuing — #14450",
                dir.join(file).display()
            ),
        }
    }
    // Non-recursive: fails (and is ignored) while reader subdirectories remain.
    let emptied = std::fs::remove_dir(dir).is_ok();
    if removed || emptied {
        log::info!(
            "credential_preflight: removed the stale per-owner credential for {owner} at {} — \
             it is no longer a cross-owner (#14450)",
            dir.display()
        );
    }
}

/// Mint and publish `plan`'s owner credential: `Ok(Some(dir))` once it is on
/// disk, `Ok(None)` when the App is not configured, `Err` (logged) when the
/// mint or the publish failed.
fn mint_and_publish(
    workspace_root: &Path,
    plan: &CrossOwnerCredentialPlan,
    minter: &dyn GithubAppMinter,
) -> Result<Option<PathBuf>, ()> {
    let owner_dir = github_app_gh_config_dir_for_owner(workspace_root, &plan.owner);
    let repo = &plan.representative_owner_repo;
    match minter.mint(repo) {
        ref outcome @ GithubAppOutcome::Minted {
            ref installation_id,
            ref app_id,
            ..
        } => match publish_outcome(&owner_dir, outcome, repo) {
            Ok(()) => {
                log::info!(
                    "credential_preflight: per-owner github-app credential established for {} \
                     ({} managed repo(s), app {app_id} installation {installation_id}) — those \
                     cross-owner repos are now reachable (#5401)",
                    plan.owner,
                    plan.roots.len()
                );
                Ok(Some(owner_dir))
            }
            Err(e) => {
                log::warn!(
                    "credential_preflight: minted a per-owner github-app token for {} but could \
                     not publish it to {} ({e}); its {} managed repo(s) remain unreachable — \
                     #5401",
                    plan.owner,
                    owner_dir.display(),
                    plan.roots.len()
                );
                Err(())
            }
        },
        GithubAppOutcome::Error(reason) => {
            log::warn!(
                "credential_preflight: could not mint a per-owner github-app token for {} \
                 ({reason}); its {} managed repo(s) ({:?}) remain unreachable — the app may not \
                 be installed on {}'s account/org (public reads may still work; private reads \
                 and writes will 404); retried with backoff — #5401/#11957",
                plan.owner,
                plan.roots.len(),
                plan.roots,
                plan.owner
            );
            Err(())
        }
        GithubAppOutcome::NotConfigured => Ok(None),
    }
}

/// Register `plan`'s roots, owner slug and refresh source against
/// `owner_dir`, expiring the write-scope answers cached under each root's
/// previous credential.
fn register_plan(plan: &CrossOwnerCredentialPlan, owner_dir: &Path) {
    let mut stale: Vec<Option<PathBuf>> = Vec::new();
    for root in &plan.roots {
        let before = gh_config_dir_for_root(root);
        if before.as_deref() == Some(owner_dir) {
            continue;
        }
        register_root_gh_config_dir(root, owner_dir);
        if !stale.contains(&before) {
            stale.push(before);
        }
    }
    // #5431: `--repo owner/repo` and `owner/repo`-in-path call sites.
    register_owner_gh_config_dir(&plan.owner, owner_dir);
    // #6171: the per-owner refresh tick re-reads this registry every tick.
    register_owner_refresh_source(&plan.representative_owner_repo, owner_dir);
    if !stale.is_empty() {
        stale.push(Some(owner_dir.to_path_buf()));
        invalidate_write_scope(&stale);
    }
}

/// Forget the installation snapshot and expire every cached write-scope
/// answer for each credential in `dirs` (`None` = the process's own), so the
/// next gate re-probes. Mirrors the stale-token retry's invalidation (#9247).
fn invalidate_write_scope(dirs: &[Option<PathBuf>]) {
    use crate::forge_repo_facts::installation;
    use crate::write_scope::probe;
    for dir in dirs {
        installation::forget(&installation::Credential::writer(dir.clone()));
        probe::expire_credential(dir.as_deref(), &probe::CacheScope::GitHub);
    }
}

/// Longest gap between retries, in primary refresh ticks (12 × ~5 min ≈ 1 h).
pub const MAX_RETRY_TICKS: u32 = 12;

/// When the primary refresh tick should (re)run [`establish_for_workspace`]
/// (#11957): on the first successful tick after a startup that never ran it,
/// then after 1, 2, 4, 8 and at most [`MAX_RETRY_TICKS`] ticks while any
/// owner keeps failing. Stops for good once a pass is complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossOwnerRetry {
    pending: bool,
    failures: u32,
    skip: u32,
}

impl CrossOwnerRetry {
    /// The state after startup: `None` when startup did not run a pass (its
    /// mint failed), else that pass's outcome.
    #[must_use]
    pub fn after_startup(pass: Option<&CrossOwnerPass>) -> Self {
        let mut retry = Self {
            pending: true,
            failures: 0,
            skip: 0,
        };
        if let Some(pass) = pass {
            retry.record(pass);
        }
        retry
    }

    /// Whether a pass is still owed at all.
    #[must_use]
    pub fn pending(&self) -> bool {
        self.pending
    }

    /// Called once per successful primary mint: whether to run a pass now.
    pub fn due(&mut self) -> bool {
        if !self.pending {
            return false;
        }
        if self.skip > 0 {
            self.skip -= 1;
            return false;
        }
        true
    }

    /// Record a pass's outcome.
    pub fn record(&mut self, pass: &CrossOwnerPass) {
        if pass.complete() {
            self.pending = false;
            self.failures = 0;
            self.skip = 0;
            return;
        }
        self.failures = self.failures.saturating_add(1);
        let gap = 1u32
            .checked_shl(self.failures - 1)
            .unwrap_or(MAX_RETRY_TICKS)
            .min(MAX_RETRY_TICKS);
        self.skip = gap - 1;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#[path = "cross_owner_tests.rs"]
mod tests;
