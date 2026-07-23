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

use std::path::Path;

/// Count the `*.token` files in `{workspace_root}/.loom/tokens/` — the size of
/// the multi-account rotation pool, and the hard ceiling on concurrent
/// autonomous sweeps.
///
/// Returns 0 when the directory is absent or unreadable (token rotation not
/// bootstrapped) — the same condition under which `spawn-claude.sh` refuses to
/// dispatch, so a 0 pool correctly yields a 0 dynamic cap (dispatch nothing that
/// could not spawn) rather than silently over-subscribing.
#[must_use]
pub fn token_pool_size(workspace_root: &Path) -> usize {
    let tokens_dir = workspace_root.join(".loom").join("tokens");
    let entries = match std::fs::read_dir(&tokens_dir) {
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
        assert_eq!(token_pool_size(tmp.path()), 3);
    }

    #[test]
    fn test_missing_dir_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        // No .loom/tokens/ at all — rotation not bootstrapped.
        assert_eq!(token_pool_size(tmp.path()), 0);
    }

    #[test]
    fn test_empty_dir_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".loom").join("tokens");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(token_pool_size(tmp.path()), 0);
    }

    #[test]
    fn test_only_non_token_files_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        write_tokens_dir(tmp.path(), &["index.json", ".bad_tokens"]);
        assert_eq!(token_pool_size(tmp.path()), 0);
    }
}
