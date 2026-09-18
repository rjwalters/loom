//! Unit tests for the workspace / pool-dir resolvers in `cli::tokens`.
//!
//! Extracted verbatim from `cli/tokens.rs` (issue #8058) for the reason
//! `scripts/check-file-size-budget.sh` names: an over-threshold file that
//! needs to grow puts the new lines in a sibling instead, and a test module
//! is the cleanest thing to move. Included from `cli/tokens.rs` with a
//! `#[path]` module declaration, so the `super::` paths these tests use
//! still resolve to `cli::tokens` exactly as before.

#[cfg(test)]
mod resolve_tokens_workspace_tests {
    //! Tests for [`resolve_tokens_workspace`] (issue #4292). No test here
    //! `chdir`s the process (unsafe to do in a parallel test binary) — the
    //! `"."` case is instead verified against a live `current_dir()` read,
    //! and the absolute-path case needs no cwd at all.
    use super::super::resolve_tokens_workspace;
    use std::path::Path;

    #[test]
    fn absolute_workspace_is_returned_unchanged() {
        let resolved = resolve_tokens_workspace("/some/repo").expect("resolve");
        assert_eq!(resolved, Path::new("/some/repo"));
    }

    /// The clap default value for every `--workspace` flag is exactly `"."`.
    /// Resolving it must equal the cwd itself — no literal trailing `.`
    /// component (the #4292 cosmetic bug: `cwd.join(".")` produces
    /// `<cwd>/.`, which then reads as `<cwd>/./.loom/tokens` in "no tokens
    /// found" warnings).
    #[test]
    fn dot_workspace_resolves_to_bare_cwd() {
        let resolved = resolve_tokens_workspace(".").expect("resolve");
        let cwd = std::env::current_dir().expect("current_dir");
        assert_eq!(resolved, cwd);
        assert!(
            !resolved.to_string_lossy().contains("/./"),
            "resolved path must not carry a literal './' component: {}",
            resolved.display()
        );
    }

    #[test]
    fn other_relative_workspace_is_joined_to_cwd() {
        let resolved = resolve_tokens_workspace("some/relative/repo").expect("resolve");
        let expected = std::env::current_dir()
            .expect("current_dir")
            .join("some/relative/repo");
        assert_eq!(resolved, expected);
    }

    /// Issue #4948: an absolute `--workspace` that already ends in
    /// `.loom/tokens` must error loudly (naming the resolved absolute path)
    /// rather than resolve — the exact "run a `tokens` subcommand from
    /// inside the pool dir" incident this issue reports.
    #[test]
    fn absolute_workspace_inside_a_pool_is_rejected() {
        let err = resolve_tokens_workspace("/repo/.loom/tokens").expect_err("must reject");
        let message = err.to_string();
        assert!(
            message.contains("/repo/.loom/tokens"),
            "error must name the resolved absolute path: {message}"
        );
    }

    /// Same guard applies to a *relative* `--workspace` that joins the cwd
    /// into a `.loom/tokens`-shaped path — the check is path-shape-based,
    /// not default-vs-explicit-based (per the issue's suggested-fix option 1).
    #[test]
    fn relative_workspace_inside_a_pool_is_rejected() {
        let err =
            resolve_tokens_workspace("some/relative/repo/.loom/tokens").expect_err("must reject");
        let cwd = std::env::current_dir().expect("current_dir");
        let expected_path = cwd.join("some/relative/repo/.loom/tokens");
        let message = err.to_string();
        assert!(
            message.contains(&expected_path.display().to_string()),
            "error must name the resolved absolute path: {message}"
        );
    }

    /// A workspace that merely *contains* `.loom/tokens` somewhere in its
    /// interior (not as its own trailing suffix) is unaffected — only an
    /// exact `.loom/tokens` suffix match is rejected.
    #[test]
    fn workspace_with_pool_dir_as_a_child_is_not_rejected() {
        let resolved =
            resolve_tokens_workspace("/repo/.loom/tokens/subdir").expect("must not reject");
        assert_eq!(resolved, Path::new("/repo/.loom/tokens/subdir"));
    }
}

#[cfg(test)]
mod resolve_tokens_pool_dir_for_cli_tests {
    //! Tests for [`resolve_tokens_pool_dir_for_cli`] (issue #4292, trip-wire
    //! 3). Like `resolve_tokens_workspace_tests`, no test here `chdir`s the
    //! process — cases that need to distinguish "cwd is/isn't a registered
    //! workspace" register the test's *actual* `current_dir()` (or a sibling
    //! of it) in a scratch `LOOM_WORKSPACES_PATH` registry instead.
    use super::super::resolve_tokens_pool_dir_for_cli;
    use loom_daemon::tokens_pool::paths::{
        per_repo_tokens_dir, shared_tokens_dir, SHARED_TOKENS_DIR_ENV,
    };
    use loom_daemon::workspace_registry::{
        normalize_path, Workspace, WorkspaceRegistry, REGISTRY_PATH_ENV,
    };
    use serial_test::serial;
    use std::fs;

    /// Mirrors how `WorkspaceRegistry::add` actually stores roots — normalized
    /// / canonicalized — so a raw `tempdir().path()` (which can differ from
    /// its canonical form, e.g. macOS `/var/folders` -> `/private/var/folders`)
    /// compares correctly against the canonicalized query path inside
    /// `resolve_client_workspace_default`.
    fn write_registry(path: &std::path::Path, roots: &[&std::path::Path]) {
        let registry = WorkspaceRegistry {
            version: 1,
            workspaces: roots
                .iter()
                .map(|r| Workspace {
                    root: normalize_path(r),
                    priority: 100,
                    config_overrides: None,
                })
                .collect(),
        };
        fs::write(path, serde_json::to_string_pretty(&registry).unwrap()).unwrap();
    }

    /// Explicit (non-`"."`) `--workspace` never consults the registry, even
    /// when one is present and would otherwise redirect to shared.
    #[test]
    #[serial]
    fn explicit_workspace_bypasses_registry_anchoring() {
        let registry_dir = tempfile::tempdir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        write_registry(&registry_dir.path().join("workspaces.json"), &[unrelated.path()]);
        std::env::set_var(REGISTRY_PATH_ENV, registry_dir.path().join("workspaces.json"));
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");

        let explicit = tempfile::tempdir().unwrap();
        let (dir, anchored) =
            resolve_tokens_pool_dir_for_cli(explicit.path().to_str().unwrap()).unwrap();
        assert_eq!(dir, per_repo_tokens_dir(explicit.path()));
        assert!(!anchored, "an explicit --workspace must never anchor to shared");

        std::env::remove_var(REGISTRY_PATH_ENV);
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    /// An empty registry (no `loom-daemon workspace add` ever run) preserves
    /// today's byte-for-byte per-repo/shared behavior for the default `"."`
    /// case too — never anchors to shared.
    #[test]
    #[serial]
    fn empty_registry_default_workspace_is_unaffected() {
        let registry_dir = tempfile::tempdir().unwrap();
        // A registry file that parses to an empty registry.
        write_registry(&registry_dir.path().join("workspaces.json"), &[]);
        std::env::set_var(REGISTRY_PATH_ENV, registry_dir.path().join("workspaces.json"));

        let (dir, anchored) = resolve_tokens_pool_dir_for_cli(".").unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(dir, loom_daemon::tokens_pool::paths::resolve_tokens_dir(&cwd));
        assert!(!anchored);

        std::env::remove_var(REGISTRY_PATH_ENV);
    }

    /// A non-empty registry that happens to register the test's own cwd
    /// resolves against that (registered) root — unchanged per-repo/shared
    /// precedence, no shared-anchoring.
    #[test]
    #[serial]
    fn non_empty_registry_with_cwd_registered_is_unaffected() {
        let cwd = std::env::current_dir().unwrap();
        let registry_dir = tempfile::tempdir().unwrap();
        write_registry(&registry_dir.path().join("workspaces.json"), &[&cwd]);
        std::env::set_var(REGISTRY_PATH_ENV, registry_dir.path().join("workspaces.json"));

        let (dir, anchored) = resolve_tokens_pool_dir_for_cli(".").unwrap();
        assert_eq!(dir, loom_daemon::tokens_pool::paths::resolve_tokens_dir(&cwd));
        assert!(!anchored);

        std::env::remove_var(REGISTRY_PATH_ENV);
    }

    /// A non-empty registry that does NOT include the test's cwd anchors the
    /// default `"."` case straight to the shared pool (the machine-level
    /// daemon / bare-cwd CLI-invocation case this trip-wire fixes).
    #[test]
    #[serial]
    fn non_empty_registry_with_cwd_unregistered_anchors_to_shared() {
        let cwd = std::env::current_dir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        assert_ne!(unrelated.path(), cwd, "sanity: must not coincide with cwd");
        let registry_dir = tempfile::tempdir().unwrap();
        write_registry(&registry_dir.path().join("workspaces.json"), &[unrelated.path()]);
        std::env::set_var(REGISTRY_PATH_ENV, registry_dir.path().join("workspaces.json"));
        std::env::remove_var(SHARED_TOKENS_DIR_ENV); // default shared dir enabled

        let (dir, anchored) = resolve_tokens_pool_dir_for_cli(".").unwrap();
        assert_eq!(dir, shared_tokens_dir().unwrap());
        assert!(anchored);

        std::env::remove_var(REGISTRY_PATH_ENV);
    }

    /// The `LOOM_SHARED_TOKENS_DIR=""` opt-out disables shared-anchoring too,
    /// not just the original per-repo/shared fallback — falls back to the
    /// per-repo(cwd) path and reports `anchored = false`.
    #[test]
    #[serial]
    fn shared_pool_disabled_falls_back_to_per_repo_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        let registry_dir = tempfile::tempdir().unwrap();
        write_registry(&registry_dir.path().join("workspaces.json"), &[unrelated.path()]);
        std::env::set_var(REGISTRY_PATH_ENV, registry_dir.path().join("workspaces.json"));
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");

        let (dir, anchored) = resolve_tokens_pool_dir_for_cli(".").unwrap();
        assert_eq!(dir, loom_daemon::tokens_pool::paths::resolve_tokens_dir(&cwd));
        assert!(!anchored);

        std::env::remove_var(REGISTRY_PATH_ENV);
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }
}
