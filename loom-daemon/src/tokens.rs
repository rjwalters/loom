//! Token-pool sizing for the autonomous work finder (#3811, Phase B of epic
//! #3809).
//!
//! The multi-account token pool lives at `{workspace}/.loom/tokens/` — one
//! `<account>.token` file per rotated Claude OAuth account, materialized by
//! `loom-tokens bootstrap` (see CLAUDE.md → "Multi-Account Token Pool"). Each
//! dispatched sweep child consumes exactly one account via `spawn-claude.sh`, so
//! the **pool size is the hard ceiling on concurrent autonomous sweeps** — a cap
//! above it would over-subscribe an account (two live sweeps sharing one OAuth
//! token, defeating the rotation that spreads weekly-limit load).
//!
//! [`token_pool_size`] counts the `*.token` files in that directory. The sibling
//! bookkeeping files the rotation logic writes there — `index.json`, `.ranking`,
//! `.allowlist`, `.bad_tokens`, `.failure_counts` — are **not** `*.token` files
//! and so are naturally excluded by the suffix filter.
//!
//! # Bad-token-aware counting is a deliberate follow-up
//!
//! A fully-accurate *usable* pool size would subtract accounts currently listed
//! in `.bad_tokens` (auth failures / exhaustion). That entails mirroring
//! `loom_tools.tokens.bad_tokens`' reason-aware TTL + word-boundary parsing —
//! meaningful scope the #3811 curator note explicitly permits deferring. This
//! first pass counts `*.token` files; the spawn path itself already skips
//! bad-marked tokens and hard-fails (`EX_CONFIG`) when the usable pool is empty,
//! so an over-count here at most defers one issue to the next tick rather than
//! over-subscribing (the claim lock + registry dedup still hold). Bad-token-aware
//! subtraction is tracked as a follow-up.

use std::path::{Path, PathBuf};

/// Env override for the shared machine-level token pool (issue #3938).
///
/// Mirrors `loom_tools.tokens.paths`: a non-empty value names the shared pool
/// directory, an explicitly empty value disables the shared fallback, and unset
/// resolves to `~/.loom/tokens`.
const SHARED_TOKENS_DIR_ENV: &str = "LOOM_SHARED_TOKENS_DIR";

/// Resolve the shared machine-level pool directory, or `None` when disabled.
///
/// Kept in lock-step with the Python resolver (`loom_tools.tokens.paths.
/// shared_tokens_dir`) so the daemon's concurrency accounting and the
/// spawn-time selector agree on where the shared pool lives.
#[must_use]
fn shared_tokens_dir() -> Option<PathBuf> {
    match std::env::var(SHARED_TOKENS_DIR_ENV) {
        Ok(v) => {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                None // explicit opt-out
            } else {
                Some(PathBuf::from(trimmed))
            }
        }
        Err(_) => dirs::home_dir().map(|h| h.join(".loom").join("tokens")),
    }
}

/// Count the `*.token` files directly under `dir` (no recursion).
fn count_token_files(dir: &Path) -> usize {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            // Count regular files whose name ends in `.token`. Dotfiles like
            // `.bad_tokens` / `.ranking` and `index.json` do not match the
            // `.token` suffix; a directory named `*.token` (there are none in
            // practice) is excluded by the file-type check.
            entry.file_type().map(|t| t.is_file()).unwrap_or(false)
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(".token"))
        })
        .count()
}

/// Count the `*.token` files in the pool that selection would actually use for
/// `workspace_root` — the size of the multi-account rotation pool, and the hard
/// ceiling on concurrent autonomous sweeps.
///
/// Resolution mirrors the Python selector (`loom_tools.tokens.paths.
/// resolve_tokens_dir`, issue #3938): the per-repo pool at
/// `{workspace_root}/.loom/tokens/` when it holds tokens, else the shared
/// machine-level pool (`~/.loom/tokens`, override `LOOM_SHARED_TOKENS_DIR`).
/// Without this fallback the multi-workspace work finder measured 0 tokens for a
/// consumer repo that had no pool of its own even though a shared pool existed —
/// the accounting the spawn path then contradicted by succeeding via the shared
/// pool.
///
/// Returns 0 when neither pool is bootstrapped — the same condition under which
/// `spawn-claude.sh` refuses to dispatch, so a 0 pool correctly yields a 0
/// dynamic cap (dispatch nothing that could not spawn) rather than silently
/// over-subscribing.
#[must_use]
pub fn token_pool_size(workspace_root: &Path) -> usize {
    token_pool_size_resolved(workspace_root, shared_tokens_dir().as_deref())
}

/// Env-free core of [`token_pool_size`]: count the per-repo pool, falling back
/// to an explicit `shared` pool directory when the per-repo pool is
/// absent/empty. Split out so it is deterministically testable without touching
/// process env or the real home directory.
#[must_use]
fn token_pool_size_resolved(workspace_root: &Path, shared: Option<&Path>) -> usize {
    let per_repo = workspace_root.join(".loom").join("tokens");
    let n = count_token_files(&per_repo);
    if n > 0 {
        return n;
    }
    // Per-repo pool absent/empty: fall back to the shared machine-level pool so
    // the concurrency ceiling matches what the spawn path can actually pick.
    shared.map_or(0, count_token_files)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

    fn write_tokens_dir(workspace: &Path, files: &[&str]) {
        let dir = workspace.join(".loom").join("tokens");
        fs::create_dir_all(&dir).unwrap();
        for f in files {
            fs::write(dir.join(f), "sk-ant-oat01-fake").unwrap();
        }
    }

    fn write_flat_pool(dir: &Path, files: &[&str]) {
        fs::create_dir_all(dir).unwrap();
        for f in files {
            fs::write(dir.join(f), "sk-ant-oat01-fake").unwrap();
        }
    }

    // The env-free core is exercised directly so tests never touch process env
    // or the real home directory (issue #3938). We pass `shared = None` to keep
    // the classic per-repo-only assertions deterministic.

    #[test]
    fn test_counts_only_token_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_tokens_dir(
            tmp.path(),
            &[
                "agent-1.token",
                "agent-2.token",
                "agent-3.token",
                // Non-token bookkeeping siblings — must NOT be counted.
                "index.json",
                ".ranking",
                ".allowlist",
                ".bad_tokens",
                ".failure_counts",
            ],
        );
        assert_eq!(token_pool_size_resolved(tmp.path(), None), 3);
    }

    #[test]
    fn test_missing_dir_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        // No .loom/tokens/ at all — rotation not bootstrapped, no shared pool.
        assert_eq!(token_pool_size_resolved(tmp.path(), None), 0);
    }

    #[test]
    fn test_empty_dir_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".loom").join("tokens");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(token_pool_size_resolved(tmp.path(), None), 0);
    }

    #[test]
    fn test_only_non_token_files_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        write_tokens_dir(tmp.path(), &["index.json", ".bad_tokens"]);
        assert_eq!(token_pool_size_resolved(tmp.path(), None), 0);
    }

    // ---- shared-pool fallback (issue #3938) ---------------------------------

    #[test]
    fn test_falls_back_to_shared_when_per_repo_absent() {
        // Consumer repo with NO per-repo pool, but a shared machine-level pool.
        let repo = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        write_flat_pool(shared.path(), &["a.token", "b.token", "index.json"]);
        assert_eq!(token_pool_size_resolved(repo.path(), Some(shared.path())), 2);
    }

    #[test]
    fn test_falls_back_to_shared_when_per_repo_empty() {
        // Per-repo dir exists but holds no .token files -> shared wins.
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".loom").join("tokens")).unwrap();
        let shared = tempfile::tempdir().unwrap();
        write_flat_pool(shared.path(), &["a.token"]);
        assert_eq!(token_pool_size_resolved(repo.path(), Some(shared.path())), 1);
    }

    #[test]
    fn test_per_repo_pool_wins_over_shared() {
        // When both exist, the per-repo pool is authoritative (no double count).
        let repo = tempfile::tempdir().unwrap();
        write_tokens_dir(repo.path(), &["r1.token", "r2.token", "r3.token"]);
        let shared = tempfile::tempdir().unwrap();
        write_flat_pool(shared.path(), &["s1.token"]);
        assert_eq!(token_pool_size_resolved(repo.path(), Some(shared.path())), 3);
    }

    #[test]
    fn test_no_pool_anywhere_is_zero() {
        let repo = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap(); // exists but empty
        assert_eq!(token_pool_size_resolved(repo.path(), Some(shared.path())), 0);
    }

    #[test]
    fn test_shared_disabled_is_zero_when_per_repo_absent() {
        // shared = None models LOOM_SHARED_TOKENS_DIR="" (operator opt-out).
        let repo = tempfile::tempdir().unwrap();
        assert_eq!(token_pool_size_resolved(repo.path(), None), 0);
    }

    #[test]
    fn test_shared_tokens_dir_env_empty_disables() {
        // Guard the process-global env mutation behind a serialized section.
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var(SHARED_TOKENS_DIR_ENV).ok();
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");
        assert!(shared_tokens_dir().is_none());
        // A non-empty value is honored verbatim.
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "/tmp/loom-shared-xyz");
        assert_eq!(shared_tokens_dir(), Some(PathBuf::from("/tmp/loom-shared-xyz")));
        match prev {
            Some(v) => std::env::set_var(SHARED_TOKENS_DIR_ENV, v),
            None => std::env::remove_var(SHARED_TOKENS_DIR_ENV),
        }
    }

    // Serialize the single env-touching test so it can't race a parallel test
    // that reads the same var.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
