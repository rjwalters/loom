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
pub const CODEX_PROFILE_ROOT_ENV: &str = "LOOM_CODEX_PROFILE_ROOT";

#[must_use]
pub fn per_repo_accounts_file(workspace: &Path) -> PathBuf {
    workspace.join(".loom").join("accounts.json")
}

/// Machine-level Codex profile root. An explicitly empty override disables it.
#[must_use]
pub fn codex_profile_root() -> Option<PathBuf> {
    match std::env::var(CODEX_PROFILE_ROOT_ENV) {
        Ok(value) if value.trim().is_empty() => None,
        Ok(value) => Some(expand_tilde(value.trim())),
        Err(_) => dirs::home_dir().map(|home| home.join(".loom").join("codex-profiles")),
    }
}

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
///   2. Default: `~/.loom/tokens` — **except under `cfg(test)`** (see below).
///
/// # Why the default is refused under `cfg(test)` (issue #4657)
///
/// `LOOM_SHARED_TOKENS_DIR` is a process-global env var, and every `#[test]`
/// in this crate's `src/` links into one multi-threaded test binary. Several
/// modules (`tokens.rs`, `ipc.rs`, `capacity.rs`, `tokens_pool/bad_tokens.rs`,
/// this module) `set_var`/`remove_var` it — `#[serial]` only serializes
/// serial-tagged tests against *each other*, so a transient window always
/// exists where a concurrent, non-serial (or differently-keyed) test observes
/// the var absent. In that window this function used to silently fall back to
/// the *real* `~/.loom/tokens`, and a test workspace with no per-repo pool
/// (e.g. `mark_bad`'s dir-missing test) would resolve straight to the
/// operator's live machine-level pool and write test fixtures into it
/// (confirmed: `agent-1`/`agent-10` fixture lines observed in a live
/// `~/.loom/tokens/.bad_tokens`). Per-test `set_var("")` guards cannot close
/// this class — the race is in *other* tests' windows, not this one's own
/// call. Refusing the default under `cfg(test)` closes it structurally: tests
/// that want to exercise the real fallback behavior must opt in explicitly via
/// `LOOM_SHARED_TOKENS_DIR=<tmp path>`, same as they already do today.
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
        #[cfg(test)]
        Err(_) => None,
        #[cfg(not(test))]
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

/// Registry-aware variant of [`resolve_tokens_dir`] for a caller whose nominal
/// workspace root is `candidate` but may not itself be a recognized Loom
/// workspace (issue #4292, trip-wires 1 & 3).
///
/// A machine-level daemon (#3835/#3926) started under systemd with a bare,
/// unconfigured cwd (e.g. `$HOME`) — or any CLI invocation that lets
/// `--workspace` default to `.` from such a cwd — resolves `candidate` to a
/// directory that is not actually a repo checkout. Feeding that straight into
/// [`resolve_tokens_dir`] is worse than merely "no tokens": because the
/// **default** shared pool is *also* `~/.loom/tokens` (see
/// [`shared_tokens_dir`]), `candidate == $HOME` makes the per-repo and shared
/// probes coincidentally check the *same* empty directory, silently masking
/// wherever the pool was actually bootstrapped (e.g. a per-repo pool at the
/// daemon's real, differently-located checkout).
///
/// This reuses the exact "is `candidate` a recognized Loom workspace"
/// question #4299 already answers for CLI `--workspace` defaulting
/// ([`crate::workspace_registry::resolve_client_workspace_default`]) rather
/// than a second, parallel detection path:
///
/// - **Registry empty** (no `loom-daemon workspace add` ever run — the
///   pre-#3926 single-workspace deployment style): trust `candidate`
///   unconditionally, i.e. byte-for-byte [`resolve_tokens_dir`]. A
///   repo-local install with no machine-level registry is never affected by
///   this function.
/// - **`candidate` falls under (or exactly at) a registered workspace root**:
///   resolve against that root's own [`resolve_tokens_dir`] precedence
///   (per-repo first, else shared) — unchanged behavior, just anchored at the
///   more precise registered root rather than a possibly-nested `candidate`.
/// - **`candidate` matches no registered root**: `candidate` is not a real
///   Loom workspace at all (the machine-level-daemon-at-`$HOME` case this
///   function exists to fix) — skip the per-repo probe entirely and resolve
///   straight to [`shared_tokens_dir`]. Falls back to
///   `resolve_tokens_dir(candidate)` only when the shared pool is itself
///   disabled (`LOOM_SHARED_TOKENS_DIR=""`), so that opt-out still disables
///   every anchoring surface, not just the original per-repo/shared fallback.
#[must_use]
pub fn resolve_tokens_dir_anchored(
    candidate: &Path,
    registry: &crate::workspace_registry::WorkspaceRegistry,
) -> PathBuf {
    if registry.workspaces.is_empty() {
        return resolve_tokens_dir(candidate);
    }
    match crate::workspace_registry::resolve_client_workspace_default(candidate, registry) {
        Some(root) => resolve_tokens_dir(&root),
        None => shared_tokens_dir().unwrap_or_else(|| resolve_tokens_dir(candidate)),
    }
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

    /// Regression test for #4657: a populated, `HOME`-overridden fake
    /// `~/.loom/tokens` (mimicking a real operator machine's live pool) must
    /// be byte-identical before and after exercising `shared_tokens_dir()`
    /// and `mark_bad()` with `LOOM_SHARED_TOKENS_DIR` unset — the exact
    /// combination (unset env var + real-looking home pool) that used to
    /// silently fall back to the default `~/.loom/tokens` and let test
    /// fixtures leak into it.
    #[test]
    #[serial]
    fn shared_tokens_dir_never_touches_a_populated_fake_home_under_test() {
        let fake_home = tempfile::tempdir().unwrap();
        let fake_shared_pool = fake_home.path().join(".loom").join("tokens");
        write_pool(&fake_shared_pool, &["real-account.token"]);
        let bad_tokens_file = fake_shared_pool.join(".bad_tokens");
        fs::write(&bad_tokens_file, "2026-01-01T00:00:00Z real-account auth\n").unwrap();
        let before = fs::read(&bad_tokens_file).unwrap();

        let prev_home = std::env::var_os("HOME");
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
        std::env::set_var("HOME", fake_home.path());

        // The env var is unset — under `cfg(test)` this must NOT fall back to
        // `~/.loom/tokens` (fake or real), unlike production behavior.
        assert_eq!(shared_tokens_dir(), None);

        // A workspace with no per-repo pool must fail closed (dir missing)
        // rather than silently resolving to the fake home's shared pool.
        let workspace = tempfile::tempdir().unwrap();
        let err = crate::tokens_pool::bad_tokens::mark_bad(workspace.path(), "agent-1", "x");
        assert!(err.is_err(), "mark_bad must fail closed, not fall back to a live pool");

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }

        let after = fs::read(&bad_tokens_file).unwrap();
        assert_eq!(
            before, after,
            "the fake ~/.loom/tokens/.bad_tokens must be byte-identical before/after"
        );
    }

    // =====================================================================
    // resolve_tokens_dir_anchored (issue #4292, trip-wires 1 & 3)
    // =====================================================================

    use crate::workspace_registry::{normalize_path, Workspace, WorkspaceRegistry};

    /// Build a test registry the same way `WorkspaceRegistry::add` would:
    /// entries store the **normalized/canonicalized** root
    /// ([`normalize_path`]), which is what `resolve_client_workspace_default`
    /// assumes when comparing against an already-normalized query path (a
    /// raw, un-canonicalized `tempdir().path()` can otherwise mismatch a
    /// symlink-resolved query, e.g. macOS `/var/folders` -> `/private/var/folders`).
    fn registry_with(roots: &[&Path]) -> WorkspaceRegistry {
        WorkspaceRegistry {
            version: 1,
            workspaces: roots
                .iter()
                .map(|r| Workspace {
                    root: normalize_path(r),
                    priority: 100,
                    config_overrides: None,
                })
                .collect(),
        }
    }

    #[test]
    #[serial]
    fn anchored_empty_registry_trusts_candidate_unchanged() {
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");
        let repo = tempfile::tempdir().unwrap();
        write_pool(&per_repo_tokens_dir(repo.path()), &["a.token"]);
        let registry = WorkspaceRegistry::default();
        assert_eq!(
            resolve_tokens_dir_anchored(repo.path(), &registry),
            per_repo_tokens_dir(repo.path())
        );
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn anchored_registered_candidate_uses_its_own_per_repo_shared_precedence() {
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");
        let repo = tempfile::tempdir().unwrap();
        write_pool(&per_repo_tokens_dir(repo.path()), &["a.token"]);
        let registry = registry_with(&[repo.path()]);
        // The resolved root is the registry's *normalized* copy of `repo.path()`
        // (e.g. macOS `/var/folders` -> `/private/var/folders`) — same
        // underlying directory, different string form.
        let canonical_repo = crate::workspace_registry::normalize_path(repo.path());
        assert_eq!(
            resolve_tokens_dir_anchored(repo.path(), &registry),
            per_repo_tokens_dir(&canonical_repo)
        );
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn anchored_unregistered_candidate_skips_straight_to_shared() {
        let repo = tempfile::tempdir().unwrap(); // registered, unrelated
        let candidate = tempfile::tempdir().unwrap(); // NOT registered (e.g. $HOME)
        let shared = tempfile::tempdir().unwrap();
        write_pool(shared.path(), &["s.token"]);
        // Even if `candidate` coincidentally has its own (empty) `.loom/tokens`
        // dir, it must never be probed once it's known not to be a workspace.
        std::env::set_var(SHARED_TOKENS_DIR_ENV, shared.path().to_str().unwrap());
        let registry = registry_with(&[repo.path()]);
        assert_eq!(resolve_tokens_dir_anchored(candidate.path(), &registry), shared.path());
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn anchored_unregistered_candidate_falls_back_to_candidate_when_shared_disabled() {
        let repo = tempfile::tempdir().unwrap();
        let candidate = tempfile::tempdir().unwrap();
        std::env::set_var(SHARED_TOKENS_DIR_ENV, ""); // opt-out
        let registry = registry_with(&[repo.path()]);
        assert_eq!(
            resolve_tokens_dir_anchored(candidate.path(), &registry),
            per_repo_tokens_dir(candidate.path())
        );
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn anchored_candidate_under_registered_root_resolves_at_the_root() {
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");
        let repo = tempfile::tempdir().unwrap();
        write_pool(&per_repo_tokens_dir(repo.path()), &["a.token"]);
        let nested = repo.path().join("subdir");
        std::fs::create_dir_all(&nested).unwrap();
        let registry = registry_with(&[repo.path()]);
        let canonical_repo = crate::workspace_registry::normalize_path(repo.path());
        // `candidate` is a subdirectory of the registered root, not the root
        // itself — resolution should still land on the registered root's pool.
        assert_eq!(
            resolve_tokens_dir_anchored(&nested, &registry),
            per_repo_tokens_dir(&canonical_repo)
        );
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }
}
