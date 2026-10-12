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

/// Env override naming the machine-level workspace root whose
/// `.loom/accounts.json` is the **shared** account registry (issue #8540).
/// An explicitly empty value disables the shared registry entirely.
pub const SHARED_ACCOUNTS_ROOT_ENV: &str = "LOOM_SHARED_ACCOUNTS_ROOT";

#[must_use]
pub fn per_repo_accounts_file(workspace: &Path) -> PathBuf {
    workspace.join(".loom").join("accounts.json")
}

/// The machine-level root whose `<root>/.loom/accounts.json` is the *shared*
/// account registry — `$HOME` by default, i.e. the same `~/.loom` that already
/// holds `codex-profiles/` and the shared token pool.
///
/// This is not a new location: `--workspace` defaulting to `.` from `$HOME`
/// already computed exactly this path (issue #8540). Naming it makes the
/// "which registry am I acting on" question answerable instead of an implicit
/// function of the operator's cwd.
///
/// Refused under `cfg(test)` for the same reason [`shared_tokens_dir`] refuses
/// its own home default (issue #4657): a test that fell back to the real
/// `$HOME` would read — and a mutating `accounts` verb would *write* — the
/// operator's live registry. Tests opt in with an explicit
/// `LOOM_SHARED_ACCOUNTS_ROOT`.
#[must_use]
pub fn shared_accounts_root() -> Option<PathBuf> {
    match std::env::var(SHARED_ACCOUNTS_ROOT_ENV) {
        Ok(value) if value.trim().is_empty() => None,
        Ok(value) => Some(expand_tilde(value.trim())),
        #[cfg(test)]
        Err(_) => None,
        #[cfg(not(test))]
        Err(_) => dirs::home_dir(),
    }
}

/// `true` iff `workspace` *is* the shared machine-level accounts root.
///
/// Compared canonically where possible so `/var/folders/...` and its
/// `/private/var/folders/...` symlink resolution (macOS) are the same answer,
/// falling back to a literal comparison for a root that does not exist.
#[must_use]
pub fn is_shared_accounts_root(workspace: &Path) -> bool {
    let Some(shared) = shared_accounts_root() else {
        return false;
    };
    match (shared.canonicalize(), workspace.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => shared == workspace,
    }
}

/// Machine-level Codex profile root. An explicitly empty override disables it.
///
/// Under `cfg(test)` an unset variable **panics** instead of resolving the real
/// `~/.loom/codex-profiles` (issue #9964): a test fixture once leaked into an
/// operator's live profile root through exactly that fallback. Tests redirect
/// the root with the test-only `tokens_pool::profile_root_env::ProfileRootEnv`.
#[must_use]
pub fn codex_profile_root() -> Option<PathBuf> {
    match std::env::var(CODEX_PROFILE_ROOT_ENV) {
        Ok(value) if value.trim().is_empty() => None,
        Ok(value) => Some(expand_tilde(value.trim())),
        #[cfg(test)]
        Err(_) => panic!(
            "{CODEX_PROFILE_ROOT_ENV} unset in a test; refusing to resolve the real ~/.loom (issue #9964)"
        ),
        #[cfg(not(test))]
        Err(_) => dirs::home_dir().map(|home| home.join(".loom").join("codex-profiles")),
    }
}

/// Return the canonical per-repo pool dir `<workspace>/.loom/tokens`.
///
/// **This location is retired for credential storage (issue #9135).** It is
/// still named here because a legacy host may have one on disk and Loom has to
/// be able to *recognize* it in order to refuse it (see
/// [`retired_in_worktree_pool`]) — not because anything should write one. Use
/// [`shared_tokens_dir`] for any provisioning destination.
#[must_use]
pub fn per_repo_tokens_dir(workspace: &Path) -> PathBuf {
    workspace.join(".loom").join("tokens")
}

/// Basename of the sentinel [`resolve_tokens_dir`] returns in place of a
/// retired in-worktree pool when the shared pool is *also* unavailable
/// (issue #9135).
///
/// Deliberately a path no provisioning flow ever creates: every
/// [`has_token_files`] probe against it answers `false`, so the caller reports
/// an empty pool and fails closed instead of readmitting the credentials the
/// refusal exists to strand. Its name is what an operator sees in that
/// "no .token files in …" message, so it has to explain itself.
pub const RETIRED_POOL_SENTINEL_NAME: &str = "tokens.retired-in-worktree";

/// The fail-closed sentinel pool path for `workspace`. See
/// [`RETIRED_POOL_SENTINEL_NAME`].
#[must_use]
pub fn retired_pool_sentinel(workspace: &Path) -> PathBuf {
    workspace.join(".loom").join(RETIRED_POOL_SENTINEL_NAME)
}

/// `true` iff `path` lies inside a git working tree — i.e. any ancestor
/// (starting with `path` itself) holds a `.git` entry.
///
/// A `.git` **directory** is an ordinary clone; a `.git` **file** is a linked
/// worktree (what `.loom/scripts/worktree.sh` creates) or a submodule. Both are
/// version-controlled trees whose contents can be staged, committed and pushed,
/// so both answer `true`. Walking every ancestor — rather than probing
/// `<path>/.git` alone — is what makes the answer right for a workspace root
/// that is a *subdirectory* of a checkout rather than the checkout root.
///
/// This is the mechanical form of the credential policy: a secret that is not
/// inside a repository cannot be committed from one
/// (`defaults/docs/credential-storage.md`).
#[must_use]
pub fn is_inside_git_worktree(path: &Path) -> bool {
    path.ancestors().any(|dir| dir.join(".git").exists())
}

/// `Some(pool)` when `workspace` holds a populated **in-worktree** token pool
/// that Loom refuses to use (issue #9135), else `None`.
///
/// "Populated" is [`has_token_files`]: an empty or absent `<workspace>/.loom/
/// tokens` is not a violation, it is simply not a pool. The refusal is reported,
/// never repaired — Loom neither reads nor deletes these credentials, because
/// silently relocating an OAuth token is not a decision a daemon gets to make.
#[must_use]
pub fn retired_in_worktree_pool(workspace: &Path) -> Option<PathBuf> {
    let pool = per_repo_tokens_dir(workspace);
    if has_token_files(&pool) && is_inside_git_worktree(workspace) {
        Some(pool)
    } else {
        None
    }
}

/// The operator-facing migration instruction for the refused in-worktree pool
/// at `pool` (issue #9135). Surfaced by every path that could otherwise have
/// used it, so "my tokens stopped being found" always arrives with its reason.
#[must_use]
pub fn in_worktree_pool_error(pool: &Path) -> String {
    format!(
        "REFUSED TOKEN POOL: {} is inside a git worktree. OAuth credentials must never live \
         inside a repository checkout (issue #9135) — a `git add -A` or a directory rename \
         defeats every .gitignore/stager guard protecting them. This pool is IGNORED, not \
         deleted: migrate it once with `mkdir -p ~/.loom/tokens && mv {}/*.token \
         ~/.loom/tokens/` (or re-provision from source with `loom-daemon tokens bootstrap`), \
         then remove {}.",
        pool.display(),
        pool.display(),
        pool.display()
    )
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
pub(super) fn expand_tilde(raw: &str) -> PathBuf {
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

/// Resolve the effective pool dir for `workspace` (issue #3938), with the
/// in-worktree pool retired (issue #9135).
///
/// Order:
///
/// 1. A populated `<workspace>/.loom/tokens` that is **not** inside a git
///    worktree — a legacy layout that is not a credential-leak vector, so it
///    still resolves (this is also the shape every fixture pool in the test
///    suite has).
/// 2. A populated [`shared_tokens_dir`] — the only *supported* location.
/// 3. Otherwise a path that holds no credentials, so the caller surfaces its
///    ordinary "run `loom-daemon tokens bootstrap`" error.
///
/// A populated `<workspace>/.loom/tokens` **inside** a git worktree is never
/// returned, not even as the last-resort path in step 3: handing it back would
/// let the very next [`has_token_files`] probe readmit the credentials this
/// refusal exists to strand. When the shared pool is disabled outright
/// (`LOOM_SHARED_TOKENS_DIR=""`) that leaves nothing to point at, so step 3
/// returns [`retired_pool_sentinel`] and the host fails closed. Callers that
/// want to *explain* the resulting empty pool ask
/// [`retired_in_worktree_pool`] + [`in_worktree_pool_error`].
#[must_use]
pub fn resolve_tokens_dir(workspace: &Path) -> PathBuf {
    let repo_dir = per_repo_tokens_dir(workspace);
    if has_token_files(&repo_dir) {
        // The git-worktree probe costs a short ancestor walk, and only a host
        // that actually has a repo-local pool ever pays for it.
        if !is_inside_git_worktree(workspace) {
            return repo_dir;
        }
        return shared_tokens_dir().unwrap_or_else(|| retired_pool_sentinel(workspace));
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

    // =====================================================================
    // Retired in-worktree pool (issue #9135)
    // =====================================================================

    /// Turn `dir` into something [`is_inside_git_worktree`] recognizes. Both
    /// shapes are exercised: `.git` as a directory (ordinary clone) and as a
    /// file (a linked worktree, which is what `worktree.sh` creates).
    fn make_git_worktree(dir: &Path, as_file: bool) {
        if as_file {
            fs::write(dir.join(".git"), "gitdir: /somewhere/.git/worktrees/issue-1\n").unwrap();
        } else {
            fs::create_dir_all(dir.join(".git")).unwrap();
        }
    }

    #[test]
    fn plain_directory_is_not_a_git_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        assert!(!is_inside_git_worktree(tmp.path()));
        assert!(!is_inside_git_worktree(&nested));
    }

    #[test]
    fn git_dir_and_git_file_both_count_as_a_worktree() {
        for as_file in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            make_git_worktree(tmp.path(), as_file);
            assert!(is_inside_git_worktree(tmp.path()), "as_file={as_file}");
        }
    }

    /// A workspace root that is a *subdirectory* of a checkout is still inside
    /// the worktree — probing `<workspace>/.git` alone would miss it.
    #[test]
    fn a_subdirectory_of_a_checkout_is_inside_the_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        make_git_worktree(tmp.path(), false);
        let nested = tmp.path().join("packages").join("app");
        fs::create_dir_all(&nested).unwrap();
        assert!(is_inside_git_worktree(&nested));
    }

    #[test]
    fn retired_pool_detected_only_when_populated_and_versioned() {
        // Populated + inside a worktree: refused.
        let repo = tempfile::tempdir().unwrap();
        make_git_worktree(repo.path(), true);
        write_pool(&per_repo_tokens_dir(repo.path()), &["a.token"]);
        assert_eq!(retired_in_worktree_pool(repo.path()), Some(per_repo_tokens_dir(repo.path())));

        // Inside a worktree but EMPTY: not a pool at all, so not a violation.
        let empty = tempfile::tempdir().unwrap();
        make_git_worktree(empty.path(), false);
        write_pool(&per_repo_tokens_dir(empty.path()), &["index.json"]);
        assert_eq!(retired_in_worktree_pool(empty.path()), None);

        // Populated but NOT version-controlled: nothing to leak.
        let plain = tempfile::tempdir().unwrap();
        write_pool(&per_repo_tokens_dir(plain.path()), &["a.token"]);
        assert_eq!(retired_in_worktree_pool(plain.path()), None);
    }

    /// The whole point: a populated in-worktree pool loses to the shared pool
    /// instead of shadowing it, whatever order they were provisioned in.
    #[test]
    #[serial]
    fn resolve_refuses_an_in_worktree_pool_in_favor_of_shared() {
        let repo = tempfile::tempdir().unwrap();
        make_git_worktree(repo.path(), true);
        write_pool(&per_repo_tokens_dir(repo.path()), &["repo.token"]);
        let shared = tempfile::tempdir().unwrap();
        write_pool(shared.path(), &["s.token"]);
        std::env::set_var(SHARED_TOKENS_DIR_ENV, shared.path().to_str().unwrap());
        assert_eq!(resolve_tokens_dir(repo.path()), shared.path());
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    /// An in-worktree pool is refused even when the shared pool is EMPTY —
    /// resolution lands on the (empty) shared pool so the caller reports "no
    /// tokens" rather than quietly using credentials from inside the checkout.
    #[test]
    #[serial]
    fn resolve_refuses_an_in_worktree_pool_even_with_an_empty_shared_pool() {
        let repo = tempfile::tempdir().unwrap();
        make_git_worktree(repo.path(), false);
        write_pool(&per_repo_tokens_dir(repo.path()), &["repo.token"]);
        let shared = tempfile::tempdir().unwrap();
        std::env::set_var(SHARED_TOKENS_DIR_ENV, shared.path().to_str().unwrap());
        let resolved = resolve_tokens_dir(repo.path());
        assert_eq!(resolved, shared.path());
        assert!(!has_token_files(&resolved));
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    /// The fail-closed edge the issue's test plan calls out: the shared pool
    /// explicitly disabled (`LOOM_SHARED_TOKENS_DIR=""`) **plus** a populated
    /// legacy in-worktree pool must NOT readmit the in-worktree path.
    #[test]
    #[serial]
    fn resolve_fails_closed_when_shared_is_disabled_and_only_a_worktree_pool_exists() {
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");
        let repo = tempfile::tempdir().unwrap();
        make_git_worktree(repo.path(), true);
        write_pool(&per_repo_tokens_dir(repo.path()), &["repo.token"]);
        let resolved = resolve_tokens_dir(repo.path());
        assert_eq!(resolved, retired_pool_sentinel(repo.path()));
        assert_ne!(resolved, per_repo_tokens_dir(repo.path()));
        assert!(!has_token_files(&resolved), "the sentinel must never hold credentials");
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    /// `resolve_tokens_dir_anchored` inherits the refusal — a registered
    /// workspace whose only pool is in-worktree must not resolve to it.
    #[test]
    #[serial]
    fn anchored_registered_candidate_also_refuses_an_in_worktree_pool() {
        let repo = tempfile::tempdir().unwrap();
        make_git_worktree(repo.path(), false);
        write_pool(&per_repo_tokens_dir(repo.path()), &["repo.token"]);
        let shared = tempfile::tempdir().unwrap();
        write_pool(shared.path(), &["s.token"]);
        std::env::set_var(SHARED_TOKENS_DIR_ENV, shared.path().to_str().unwrap());
        let registry = registry_with(&[repo.path()]);
        assert_eq!(resolve_tokens_dir_anchored(repo.path(), &registry), shared.path());
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    /// The refusal message has to be actionable: it names the offending pool
    /// and the migration, and says the pool was ignored rather than deleted.
    #[test]
    fn in_worktree_pool_error_is_actionable() {
        let pool = Path::new("/repo/.loom/tokens");
        let msg = in_worktree_pool_error(pool);
        assert!(msg.contains("/repo/.loom/tokens"), "names the pool: {msg}");
        assert!(msg.contains("#9135"), "cites the policy: {msg}");
        assert!(msg.contains("~/.loom/tokens"), "names the destination: {msg}");
        assert!(msg.contains("IGNORED, not deleted"), "says it was not deleted: {msg}");
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
    // shared account registry (issue #8540)
    // =====================================================================

    #[test]
    #[serial]
    fn shared_accounts_root_defaults_to_none_under_test() {
        std::env::remove_var(SHARED_ACCOUNTS_ROOT_ENV);
        assert_eq!(shared_accounts_root(), None);
    }

    #[test]
    #[serial]
    fn shared_accounts_root_is_disabled_by_an_empty_override() {
        std::env::set_var(SHARED_ACCOUNTS_ROOT_ENV, "");
        assert_eq!(shared_accounts_root(), None);
        std::env::remove_var(SHARED_ACCOUNTS_ROOT_ENV);
    }

    #[test]
    #[serial]
    fn shared_accounts_root_honors_an_explicit_path() {
        let shared = tempfile::tempdir().unwrap();
        std::env::set_var(SHARED_ACCOUNTS_ROOT_ENV, shared.path());
        assert_eq!(shared_accounts_root().as_deref(), Some(shared.path()));
        assert!(is_shared_accounts_root(shared.path()));
        let other = tempfile::tempdir().unwrap();
        assert!(!is_shared_accounts_root(other.path()));
        std::env::remove_var(SHARED_ACCOUNTS_ROOT_ENV);
    }

    /// The shared registry file is the same `<root>/.loom/accounts.json` shape
    /// a per-repo workspace uses — only the root differs.
    #[test]
    #[serial]
    fn shared_registry_file_uses_the_per_repo_shape() {
        std::env::set_var(SHARED_ACCOUNTS_ROOT_ENV, "/tmp/loom-shared-home");
        let root = shared_accounts_root().unwrap();
        assert_eq!(
            per_repo_accounts_file(&root),
            PathBuf::from("/tmp/loom-shared-home/.loom/accounts.json")
        );
        std::env::remove_var(SHARED_ACCOUNTS_ROOT_ENV);
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
                    maintain_only: None,
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
