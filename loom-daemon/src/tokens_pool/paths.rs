//! Shared resolution of the effective token-pool directory (issue #3938).
//!
//! Ports `loom_tools.tokens.paths` byte-for-byte in semantics. See that
//! module's docstring for the full "why" (one-daemon-many-repos cross-repo
//! dispatch motivation); this is the mechanical Rust port.
//!
//! A near-identical (but private, capacity-sizing-only) resolver already
//! lives in [`crate::tokens`]. That module predates this port (#3811) and
//! solves a narrower problem (counting `*.token` files for the dynamic
//! concurrency cap); this module is the full parity port consumed by
//! [`super::select`], [`super::bad_tokens`], [`super::allowlist`], and
//! [`super::failure_counts`]. Keep both resolvers in lock-step with the
//! Python source of truth if either changes.

use std::path::{Path, PathBuf};

/// Env override for the shared machine-level pool location. Mirrors
/// `loom_tools.tokens.paths.SHARED_TOKENS_DIR_ENV`.
pub const SHARED_TOKENS_DIR_ENV: &str = "LOOM_SHARED_TOKENS_DIR";

/// Return the canonical per-repo pool dir `<workspace>/.loom/tokens`.
#[must_use]
pub fn per_repo_tokens_dir(workspace: &Path) -> PathBuf {
    workspace.join(".loom").join("tokens")
}

/// Return the shared machine-level pool dir, or `None` when disabled.
///
/// Resolution precedence (highest first):
///   1. `LOOM_SHARED_TOKENS_DIR` env var — non-empty names the dir (`~`
///      expanded); explicitly empty disables the shared fallback.
///   2. Default: `~/.loom/tokens`.
#[must_use]
pub fn shared_tokens_dir() -> Option<PathBuf> {
    match std::env::var(SHARED_TOKENS_DIR_ENV) {
        Ok(v) => {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(expand_tilde(trimmed))
            }
        }
        Err(_) => dirs::home_dir().map(|h| h.join(".loom").join("tokens")),
    }
}

/// Minimal `~`/`~/` expansion (Python's `Path.expanduser()` equivalent for
/// the cases this env var realistically carries).
fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(raw)
}

/// Return `true` iff `tokens_dir` exists and holds at least one `*.token` file.
#[must_use]
pub fn has_token_files(tokens_dir: &Path) -> bool {
    let entries = match std::fs::read_dir(tokens_dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    entries.filter_map(Result::ok).any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with(".token"))
    })
}

/// Resolve the effective pool dir for `workspace` (issue #3938).
///
/// Returns the per-repo pool when it holds `*.token` files, else the shared
/// machine-level pool when *it* holds token files, else the per-repo path
/// unchanged (so callers surface a sensible "run `loom-tokens bootstrap`"
/// error against the repo they were invoked from).
#[must_use]
pub fn resolve_tokens_dir(workspace: &Path) -> PathBuf {
    let repo_dir = per_repo_tokens_dir(workspace);
    if has_token_files(&repo_dir) {
        return repo_dir;
    }
    if let Some(shared) = shared_tokens_dir() {
        if has_token_files(&shared) {
            return shared;
        }
    }
    repo_dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_pool(dir: &Path, files: &[&str]) {
        fs::create_dir_all(dir).unwrap();
        for f in files {
            fs::write(dir.join(f), "sk-ant-oat01-fake").unwrap();
        }
    }

    // `SHARED_TOKENS_DIR_ENV` is process-global, and this same var is also
    // mutated by tests in `select.rs`. `#[serial]` (serial_test's default,
    // unkeyed group) serializes against *every* other unkeyed `#[serial]`
    // test in the crate, not just this module — the cross-module guarantee
    // a private mutex here couldn't provide.
    use serial_test::serial;

    #[test]
    fn per_repo_dir_is_dot_loom_tokens() {
        let ws = Path::new("/tmp/example-repo");
        assert_eq!(per_repo_tokens_dir(ws), PathBuf::from("/tmp/example-repo/.loom/tokens"));
    }

    #[test]
    fn has_token_files_false_for_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!has_token_files(&tmp.path().join("nope")));
    }

    #[test]
    fn has_token_files_ignores_dotfiles() {
        let tmp = tempfile::tempdir().unwrap();
        write_pool(tmp.path(), &["index.json", ".ranking", ".bad_tokens"]);
        assert!(!has_token_files(tmp.path()));
    }

    #[test]
    fn has_token_files_true_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        write_pool(tmp.path(), &["a.token"]);
        assert!(has_token_files(tmp.path()));
    }

    #[test]
    #[serial]
    fn resolve_prefers_per_repo_pool() {
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");
        let repo = tempfile::tempdir().unwrap();
        write_pool(&per_repo_tokens_dir(repo.path()), &["a.token"]);
        assert_eq!(resolve_tokens_dir(repo.path()), per_repo_tokens_dir(repo.path()));
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn resolve_falls_back_to_shared_when_per_repo_empty() {
        let repo = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        write_pool(shared.path(), &["s.token"]);
        std::env::set_var(SHARED_TOKENS_DIR_ENV, shared.path().to_str().unwrap());
        assert_eq!(resolve_tokens_dir(repo.path()), shared.path());
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn resolve_returns_per_repo_path_when_neither_has_tokens() {
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");
        let repo = tempfile::tempdir().unwrap();
        assert_eq!(resolve_tokens_dir(repo.path()), per_repo_tokens_dir(repo.path()));
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn shared_dir_disabled_by_empty_env() {
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");
        assert_eq!(shared_tokens_dir(), None);
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn shared_dir_honors_explicit_path() {
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "/tmp/loom-shared-xyz");
        assert_eq!(shared_tokens_dir(), Some(PathBuf::from("/tmp/loom-shared-xyz")));
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }
}
