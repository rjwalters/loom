//! Resolve the base directory that holds Loom worktrees.
//!
//! This is the Rust port of `defaults/scripts/lib/worktree-root.sh`
//! (`loom_worktree_root`). It mirrors that helper's resolution precedence,
//! repo-basename namespacing, and relative-override fallback **exactly** so the
//! daemon's terminal-destroy GC recognizes worktrees on an overridden base
//! (e.g. an external volume) the same way the bash tooling does (#3536, follow-up
//! to #3530).
//!
//! Resolution precedence (first match wins), all opt-in:
//!
//! 1. `LOOM_WORKTREE_ROOT` env var          — highest priority
//! 2. `.loom/config.json` → `worktree.root` — soft-fail serde_json read
//! 3. `${repo_root}/.loom/worktrees`         — default, UNCHANGED behavior
//!
//! When an override (env var or config key) is set, the returned path is
//! namespaced by repo basename so multiple workspaces can share one external
//! volume without colliding (`${override}/<repo-basename>`).
//!
//! With neither override set, the function returns `${repo_root}/.loom/worktrees`
//! verbatim — byte-for-byte identical to the historical hardcoded path, so
//! default installations see zero behavior change.
//!
//! A RELATIVE override (env var or config key) is rejected with a warning log
//! and the function falls back to the default (matching bash's
//! stderr-warning-and-fallback behavior, not a hard error). An external worktree
//! root must be absolute so that cleanup/GC comparison sites (which compare
//! absolute paths) match.

use std::path::{Path, PathBuf};

/// Resolve the absolute worktree base directory for `repo_root`.
///
/// `repo_root` must be an absolute path to the main workspace (the parent of the
/// git common dir). Callers append `issue-<N>` / `pr-<N>` as before.
///
/// Mirrors `loom_worktree_root` in `defaults/scripts/lib/worktree-root.sh`:
/// env var > `.loom/config.json` `worktree.root` > `${repo_root}/.loom/worktrees`,
/// with repo-basename namespacing on override and relative-override fallback.
pub fn worktree_root(repo_root: &Path) -> PathBuf {
    let default = || repo_root.join(".loom").join("worktrees");

    // 1. Env var override — highest priority.
    if let Ok(env_root) = std::env::var("LOOM_WORKTREE_ROOT") {
        if !env_root.is_empty() {
            if Path::new(&env_root).is_absolute() {
                return namespaced(&env_root, repo_root);
            }
            log::warn!(
                "LOOM_WORKTREE_ROOT must be an absolute path (got: '{env_root}'); falling back to default"
            );
            return default();
        }
    }

    // 2. Config key override — .loom/config.json → worktree.root.
    //    Soft-fail serde_json read, mirroring extract_configured_terminal_ids.
    if let Some(cfg_root) = read_config_worktree_root(repo_root) {
        if Path::new(&cfg_root).is_absolute() {
            return namespaced(&cfg_root, repo_root);
        }
        log::warn!(
            "worktree.root in .loom/config.json must be an absolute path (got: '{cfg_root}'); falling back to default"
        );
        return default();
    }

    // 3. Default — unchanged historical behavior.
    default()
}

/// Namespace an absolute override root by the repo basename.
///
/// Mirrors bash `${override%/}/<repo-basename>`: strip a trailing slash from the
/// override, then join the repo's basename. If `repo_root` has no final
/// component (e.g. `/`), fall back to joining nothing extra.
fn namespaced(override_root: &str, repo_root: &Path) -> PathBuf {
    // Strip trailing slash(es) to match bash `${override%/}` (single trailing
    // slash), but PathBuf::join already normalizes, so trim_end_matches keeps
    // parity for the common cases.
    let trimmed = override_root.trim_end_matches('/');
    let base = PathBuf::from(trimmed);
    match repo_root.file_name() {
        Some(name) => base.join(name),
        None => base,
    }
}

/// Read `.loom/config.json` → `worktree.root`, soft-failing to `None`.
///
/// Follows the pattern in `crate::extract_configured_terminal_ids`: missing
/// file, parse error, or missing key all resolve to `None` (never a hard error).
fn read_config_worktree_root(repo_root: &Path) -> Option<String> {
    let config_path = repo_root.join(".loom").join("config.json");

    let config_str = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) => {
            log::debug!("Could not read config at {}: {e}", config_path.display());
            return None;
        }
    };

    let config: serde_json::Value = match serde_json::from_str(&config_str) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("Could not parse config at {}: {e}", config_path.display());
            return None;
        }
    };

    let root = config.get("worktree")?.get("root")?.as_str()?;
    if root.is_empty() {
        return None;
    }
    Some(root.to_string())
}

/// Whether `path` is a Loom-managed worktree eligible for GC.
///
/// Two-way match mirroring `defaults/scripts/agent-destroy.sh`: a path counts if
/// it lives under the resolved worktree root for `repo_root` (override-aware) OR
/// contains the historical `.loom/worktrees` substring. The substring branch
/// preserves default-path detection unchanged and covers mixed setups where an
/// override was configured after worktrees were already created under the
/// default base.
pub fn is_worktree_path(path: &Path, repo_root: &Path) -> bool {
    path.starts_with(worktree_root(repo_root)) || path.to_string_lossy().contains(".loom/worktrees")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // std::env::set_var mutates process-global state; serialize env-touching
    // tests so parallel execution doesn't race on LOOM_WORKTREE_ROOT.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Run `f` with LOOM_WORKTREE_ROOT set to `value` (or unset if None),
    /// restoring the prior value afterward. Serialized via ENV_LOCK.
    fn with_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("LOOM_WORKTREE_ROOT").ok();
        match value {
            Some(v) => std::env::set_var("LOOM_WORKTREE_ROOT", v),
            None => std::env::remove_var("LOOM_WORKTREE_ROOT"),
        }
        let result = f();
        match prev {
            Some(p) => std::env::set_var("LOOM_WORKTREE_ROOT", p),
            None => std::env::remove_var("LOOM_WORKTREE_ROOT"),
        }
        result
    }

    fn write_config(dir: &Path, body: &str) {
        let loom_dir = dir.join(".loom");
        fs::create_dir_all(&loom_dir).unwrap();
        fs::write(loom_dir.join("config.json"), body).unwrap();
    }

    use std::fs;

    #[test]
    fn default_when_no_override() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("my-repo");
        fs::create_dir_all(&repo_root).unwrap();

        with_env(None, || {
            let got = worktree_root(&repo_root);
            assert_eq!(got, repo_root.join(".loom").join("worktrees"));
        });
    }

    #[test]
    fn env_override_namespaces_by_basename() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("my-repo");
        fs::create_dir_all(&repo_root).unwrap();

        with_env(Some("/Volumes/Stripe"), || {
            let got = worktree_root(&repo_root);
            assert_eq!(got, PathBuf::from("/Volumes/Stripe/my-repo"));
        });
    }

    #[test]
    fn env_override_strips_trailing_slash() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("my-repo");
        fs::create_dir_all(&repo_root).unwrap();

        with_env(Some("/Volumes/Stripe/"), || {
            let got = worktree_root(&repo_root);
            assert_eq!(got, PathBuf::from("/Volumes/Stripe/my-repo"));
        });
    }

    #[test]
    fn config_override_namespaces_by_basename() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("my-repo");
        fs::create_dir_all(&repo_root).unwrap();
        write_config(&repo_root, r#"{"worktree": {"root": "/Volumes/Ext"}}"#);

        with_env(None, || {
            let got = worktree_root(&repo_root);
            assert_eq!(got, PathBuf::from("/Volumes/Ext/my-repo"));
        });
    }

    #[test]
    fn env_beats_config() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("my-repo");
        fs::create_dir_all(&repo_root).unwrap();
        write_config(&repo_root, r#"{"worktree": {"root": "/Volumes/Config"}}"#);

        with_env(Some("/Volumes/Env"), || {
            let got = worktree_root(&repo_root);
            assert_eq!(got, PathBuf::from("/Volumes/Env/my-repo"));
        });
    }

    #[test]
    fn relative_env_override_falls_back_to_default() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("my-repo");
        fs::create_dir_all(&repo_root).unwrap();

        with_env(Some("relative/path"), || {
            let got = worktree_root(&repo_root);
            assert_eq!(got, repo_root.join(".loom").join("worktrees"));
        });
    }

    #[test]
    fn relative_config_override_falls_back_to_default() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("my-repo");
        fs::create_dir_all(&repo_root).unwrap();
        write_config(&repo_root, r#"{"worktree": {"root": "relative/path"}}"#);

        with_env(None, || {
            let got = worktree_root(&repo_root);
            assert_eq!(got, repo_root.join(".loom").join("worktrees"));
        });
    }

    #[test]
    fn empty_env_override_falls_back_to_config_then_default() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("my-repo");
        fs::create_dir_all(&repo_root).unwrap();

        // Empty env var is treated as unset → falls through to default here.
        with_env(Some(""), || {
            let got = worktree_root(&repo_root);
            assert_eq!(got, repo_root.join(".loom").join("worktrees"));
        });
    }

    #[test]
    fn missing_config_key_uses_default() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("my-repo");
        fs::create_dir_all(&repo_root).unwrap();
        // Config exists but has no worktree.root key.
        write_config(&repo_root, r#"{"terminals": []}"#);

        with_env(None, || {
            let got = worktree_root(&repo_root);
            assert_eq!(got, repo_root.join(".loom").join("worktrees"));
        });
    }

    #[test]
    fn malformed_config_uses_default() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("my-repo");
        fs::create_dir_all(&repo_root).unwrap();
        write_config(&repo_root, "{not valid json");

        with_env(None, || {
            let got = worktree_root(&repo_root);
            assert_eq!(got, repo_root.join(".loom").join("worktrees"));
        });
    }

    #[test]
    fn gate_matches_default_path_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("my-repo");
        fs::create_dir_all(&repo_root).unwrap();

        with_env(None, || {
            let wt = repo_root.join(".loom").join("worktrees").join("issue-42");
            assert!(is_worktree_path(&wt, &repo_root));
        });
    }

    #[test]
    fn gate_matches_override_path_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("my-repo");
        fs::create_dir_all(&repo_root).unwrap();

        with_env(Some("/Volumes/Stripe"), || {
            let wt = PathBuf::from("/Volumes/Stripe/my-repo/issue-42");
            assert!(is_worktree_path(&wt, &repo_root));
        });
    }

    #[test]
    fn gate_matches_default_substring_even_with_override_set() {
        // Mixed setup: an override is configured, but a worktree still lives
        // under the historical .loom/worktrees base. The substring fallback
        // must still match it.
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("my-repo");
        fs::create_dir_all(&repo_root).unwrap();

        with_env(Some("/Volumes/Stripe"), || {
            let wt = repo_root.join(".loom").join("worktrees").join("issue-99");
            assert!(is_worktree_path(&wt, &repo_root));
        });
    }

    #[test]
    fn gate_rejects_unrelated_path() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("my-repo");
        fs::create_dir_all(&repo_root).unwrap();

        with_env(Some("/Volumes/Stripe"), || {
            let unrelated = PathBuf::from("/some/other/place/issue-42");
            assert!(!is_worktree_path(&unrelated, &repo_root));
        });
    }
}
