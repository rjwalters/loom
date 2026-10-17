//! Prune stray Codex profile directories from the host-wide profile root
//! (issue #12460, slice of #12453).
//!
//! `~/.loom/codex-profiles/` is host-wide and nothing removed directories from
//! it: one host carried 19 GB there. A profile directory is credential-bearing
//! (`auth.json`), so this pass is deliberately narrower than the cache GCs
//! beside it ([`crate::target_dir_gc`], [`crate::tmpfs_reclaim`]). A directory
//! is removed only when **every** gate holds:
//!
//! 1. **Unregistered.** The workspace has its own `.loom/accounts.json`, and
//!    neither it nor the shared registry names the directory
//!    ([`unregistered_codex_profiles`]). No registry file means the discovered
//!    directories *are* the adopted pool, so nothing is pruned.
//! 2. **Idle.** Nothing under it was modified within [`MIN_IDLE_DAYS`]; a
//!    profile mid-provisioning or mid-session is never idle.
//! 3. **Not in use.** No process of this user has `CODEX_HOME` pointing at it
//!    (a running sweep). If `/proc` cannot be read, every directory counts as
//!    in use, so a non-Linux host prunes nothing.
//!
//! Hidden directories (`.quarantine`) and registered profiles are never
//! candidates. Growth *inside* a registered profile (`sessions/` rollouts) is
//! out of scope here and is left alone.
//!
//! uv's cache is intentionally not pruned by the daemon: its bound is a host
//! tool setting provisioned from the worker spec, documented in
//! `troubleshooting.md` → "Bounding the uv cache".

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::tokens_pool::account_registry::unregistered_codex_profiles;

/// A candidate must have had no modification anywhere under it for this long.
pub const MIN_IDLE_DAYS: u64 = 14;

/// What one pass did, for logging and tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CacheReapReport {
    pub removed: Vec<PathBuf>,
    pub kept: Vec<(PathBuf, &'static str)>,
}

/// Newest mtime anywhere under `path` (symlinks are not followed).
fn newest_mtime(path: &Path) -> io::Result<SystemTime> {
    let meta = fs::symlink_metadata(path)?;
    let mut newest = meta.modified()?;
    if meta.is_dir() {
        for entry in fs::read_dir(path)?.flatten() {
            if let Ok(child) = newest_mtime(&entry.path()) {
                newest = newest.max(child);
            }
        }
    }
    Ok(newest)
}

/// `CODEX_HOME` values in the environments of this user's processes, or `None`
/// when `/proc` cannot be enumerated (callers must then assume "in use").
fn running_codex_homes() -> Option<Vec<PathBuf>> {
    let mut homes = Vec::new();
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            continue;
        }
        // Other users' environ is unreadable (and cannot be our sweeps').
        let Ok(environ) = fs::read(entry.path().join("environ")) else {
            continue;
        };
        for var in environ.split(|b| *b == 0) {
            if let Some(value) = var.strip_prefix(b"CODEX_HOME=") {
                let value = PathBuf::from(String::from_utf8_lossy(value).into_owned());
                homes.push(value.canonicalize().unwrap_or(value));
            }
        }
    }
    Some(homes)
}

/// Pure-ish core: decide and remove. `in_use` is `None` when liveness of
/// processes is unknown, which keeps everything.
fn prune(
    candidates: &[(String, PathBuf)],
    now: SystemTime,
    in_use: Option<&[PathBuf]>,
    remove: &dyn Fn(&Path) -> io::Result<()>,
) -> CacheReapReport {
    let mut report = CacheReapReport::default();
    let min_idle = Duration::from_secs(MIN_IDLE_DAYS * 24 * 60 * 60);
    for (_, dir) in candidates {
        let Some(in_use) = in_use else {
            report.kept.push((dir.clone(), "process liveness unknown"));
            continue;
        };
        if in_use.iter().any(|home| home.starts_with(dir)) {
            report
                .kept
                .push((dir.clone(), "CODEX_HOME of a running process"));
            continue;
        }
        let idle = newest_mtime(dir)
            .ok()
            .and_then(|newest| now.duration_since(newest).ok())
            .is_some_and(|age| age >= min_idle);
        if !idle {
            report
                .kept
                .push((dir.clone(), "recent or unreadable activity"));
            continue;
        }
        match remove(dir) {
            Ok(()) => report.removed.push(dir.clone()),
            Err(_) => report.kept.push((dir.clone(), "removal failed")),
        }
    }
    report
}

/// One production pass for `repo_root`'s registry. Never fails the reaper tick:
/// any error resolving the registry or root prunes nothing.
pub fn run_for(repo_root: &Path) -> CacheReapReport {
    let candidates = match unregistered_codex_profiles(repo_root) {
        Ok(candidates) if !candidates.is_empty() => candidates,
        Ok(_) => return CacheReapReport::default(),
        Err(error) => {
            log::debug!("cache_reaper: registry unreadable, pruning nothing: {error:#}");
            return CacheReapReport::default();
        }
    };
    let homes = running_codex_homes();
    let report =
        prune(&candidates, SystemTime::now(), homes.as_deref(), &|dir| fs::remove_dir_all(dir));
    for dir in &report.removed {
        log::info!("cache_reaper: removed unregistered codex profile {}", dir.display());
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens_pool::paths::SHARED_ACCOUNTS_ROOT_ENV;
    use crate::tokens_pool::profile_root_env::ProfileRootEnv;
    use serial_test::serial;

    const REGISTRY: &str = r#"{"version":1,"accounts":[
        {"provider":"codex","name":"live","credential_kind":"codex_home","credential_reference":"live"}]}"#;

    struct Fixture {
        workspace: tempfile::TempDir,
        root: tempfile::TempDir,
        _shared: tempfile::TempDir,
        _env: ProfileRootEnv,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::env::remove_var(SHARED_ACCOUNTS_ROOT_ENV);
        }
    }

    fn fixture(registry: Option<&str>) -> Fixture {
        let workspace = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        let env = ProfileRootEnv::set(root.path());
        std::env::set_var(SHARED_ACCOUNTS_ROOT_ENV, shared.path());
        if let Some(body) = registry {
            fs::create_dir_all(workspace.path().join(".loom")).unwrap();
            fs::write(workspace.path().join(".loom/accounts.json"), body).unwrap();
        }
        for name in ["live", "stray", ".quarantine"] {
            fs::create_dir_all(root.path().join(name)).unwrap();
            fs::write(root.path().join(name).join("auth.json"), "{}").unwrap();
        }
        Fixture {
            workspace,
            root,
            _shared: shared,
            _env: env,
        }
    }

    fn candidates(f: &Fixture) -> Vec<(String, PathBuf)> {
        unregistered_codex_profiles(f.workspace.path()).unwrap()
    }

    fn remove_all(dir: &Path) -> io::Result<()> {
        fs::remove_dir_all(dir)
    }

    fn far_future() -> SystemTime {
        SystemTime::now() + Duration::from_secs(60 * 24 * 60 * 60)
    }

    #[test]
    #[serial]
    fn unregistered_idle_profile_is_removed_registered_and_hidden_are_kept() {
        let f = fixture(Some(REGISTRY));
        let report = prune(&candidates(&f), far_future(), Some(&[]), &remove_all);
        assert_eq!(report.removed, vec![f.root.path().canonicalize().unwrap().join("stray")]);
        assert!(!f.root.path().join("stray").exists());
        assert!(f.root.path().join("live").join("auth.json").exists());
        assert!(f.root.path().join(".quarantine").join("auth.json").exists());
    }

    #[test]
    #[serial]
    fn no_registry_file_means_nothing_is_a_candidate() {
        let f = fixture(None);
        assert!(candidates(&f).is_empty());
    }

    #[test]
    #[serial]
    fn disabled_root_prunes_nothing() {
        let f = fixture(Some(REGISTRY));
        let _off = ProfileRootEnv::set("");
        assert!(candidates(&f).is_empty());
    }

    #[test]
    #[serial]
    fn recent_activity_in_use_and_unknown_liveness_keep_the_profile() {
        let f = fixture(Some(REGISTRY));
        let c = candidates(&f);
        let fresh = prune(&c, SystemTime::now(), Some(&[]), &remove_all);
        assert!(fresh.removed.is_empty(), "fresh profile must be kept");
        let home = c[0].1.join("sessions");
        let busy = prune(&c, far_future(), Some(&[home]), &remove_all);
        assert!(busy.removed.is_empty(), "profile with a live CODEX_HOME must be kept");
        let unknown = prune(&c, far_future(), None, &remove_all);
        assert!(unknown.removed.is_empty(), "unknown liveness must keep everything");
        assert!(f.root.path().join("stray").exists());
    }

    #[test]
    #[serial]
    fn name_in_the_shared_registry_is_not_a_candidate() {
        let f = fixture(Some(REGISTRY));
        let shared = std::env::var(SHARED_ACCOUNTS_ROOT_ENV).unwrap();
        fs::create_dir_all(Path::new(&shared).join(".loom")).unwrap();
        let body = REGISTRY.replace("live", "stray");
        fs::write(Path::new(&shared).join(".loom/accounts.json"), body).unwrap();
        assert!(candidates(&f).is_empty());
    }
}
