//! `loom-daemon tokens` / `tokens pin` / `claude-config` handlers (Issue
//! #4712 — split out of `main.rs`).

use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};

use crate::{ClaudeConfigAction, ForgeAction, PinAction, TokensAction};

/// Resolve the `--workspace` flag to an absolute path. No upward `.git`
/// walk (unlike Python's `find_repo_root()`) — a deliberate Phase-1
/// simplification since this CLI has no existing callers yet; pass the
/// canonical repo root explicitly (as `spawn-claude.sh` already resolves via
/// `git rev-parse --git-common-dir` for the Python selector).
///
/// Refuses (issue #4948) a resolved workspace whose path already ends in
/// `.loom/tokens` — every `tokens` subcommand joins `.loom/tokens` onto the
/// resolved workspace to find its pool dir, so pointing `--workspace` (or
/// letting the `"."` default resolve via a cwd) *at* an existing pool would
/// otherwise silently nest a second pool at
/// `<pool>/.loom/tokens/.loom/tokens` and still report success. The check is
/// path-shape-based, not default-vs-explicit — it applies to an explicit
/// `--workspace /some/path/.loom/tokens` exactly the same as the `"."`
/// default resolving to a cwd of `/some/path/.loom/tokens`.
pub(crate) fn resolve_tokens_workspace(workspace: &str) -> Result<PathBuf> {
    let p = Path::new(workspace);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else if p == Path::new(".") {
        // The common case (every `--workspace` flag defaults to `"."`): return
        // the cwd itself rather than `cwd.join(".")`, which `PathBuf::join`
        // does not normalize away — it appends a literal trailing `Component::
        // CurDir`, producing paths like `/home/ubuntu/./.loom/tokens` in
        // "no tokens found" warnings (issue #4292). Functionally identical
        // either way; this just keeps resolved/printed paths readable.
        std::env::current_dir()?
    } else {
        std::env::current_dir()?.join(p)
    };
    reject_workspace_inside_pool(&resolved)?;
    Ok(resolved)
}

/// Refuse a resolved workspace that already ends in `.loom/tokens` (issue
/// #4948, suggested-fix option 1). See [`resolve_tokens_workspace`] for the
/// full rationale — this is a pure path-shape check, not a filesystem probe,
/// so it also catches a workspace that does not exist yet.
fn reject_workspace_inside_pool(resolved: &Path) -> Result<()> {
    let pool_suffix = Path::new(".loom").join("tokens");
    if resolved.ends_with(&pool_suffix) {
        bail!(
            "refusing to use {} as a --workspace: it is already a token pool directory (or \
             exactly matches the pool path shape `.loom/tokens`). Using it as a workspace would \
             silently nest a second pool at {}. Pass the enclosing repo root instead{}.",
            resolved.display(),
            resolved.join(&pool_suffix).display(),
            resolved
                .parent()
                .and_then(Path::parent)
                .map_or_else(String::new, |root| format!(" (e.g. --workspace {})", root.display()))
        );
    }
    Ok(())
}

/// `loom-daemon claude-config` handler (issue #4415).
pub(crate) fn handle_claude_config_command(action: ClaudeConfigAction) -> Result<()> {
    use loom_daemon::terminal::{
        claude_config_cleanup, claude_config_setup, claude_config_trust, claude_config_validate,
    };

    match action {
        ClaudeConfigAction::Setup {
            name,
            workspace,
            json,
        } => {
            let repo_root = resolve_tokens_workspace(&workspace)?;
            match claude_config_setup(&name, &repo_root) {
                Some(config_dir) => {
                    if json {
                        let mut obj = serde_json::Map::new();
                        obj.insert(
                            "config_dir".to_string(),
                            serde_json::Value::String(config_dir.display().to_string()),
                        );
                        println!("{}", serde_json::Value::Object(obj));
                    } else {
                        println!("{}", config_dir.display());
                    }
                    Ok(())
                }
                None => {
                    eprintln!("error: failed to set up config dir for '{name}'");
                    std::process::exit(1);
                }
            }
        }

        ClaudeConfigAction::Cleanup {
            name,
            workspace,
            json,
        } => {
            let repo_root = resolve_tokens_workspace(&workspace)?;
            let removed = claude_config_cleanup(&name, &repo_root);
            if json {
                let mut obj = serde_json::Map::new();
                obj.insert("removed".to_string(), serde_json::Value::Bool(removed));
                println!("{}", serde_json::Value::Object(obj));
            } else if removed {
                println!("Removed config dir for '{name}'");
            } else {
                println!("No config dir found for '{name}' (nothing to remove)");
            }
            Ok(())
        }

        ClaudeConfigAction::Validate {
            name,
            workspace,
            json,
        } => {
            let repo_root = resolve_tokens_workspace(&workspace)?;
            let healthy = claude_config_validate(&name, &repo_root);
            if json {
                let mut obj = serde_json::Map::new();
                obj.insert("healthy".to_string(), serde_json::Value::Bool(healthy));
                println!("{}", serde_json::Value::Object(obj));
            }
            if healthy {
                Ok(())
            } else {
                if !json {
                    eprintln!("config dir for '{name}' is missing or unhealthy");
                }
                std::process::exit(1);
            }
        }

        ClaudeConfigAction::Trust { project_dir } => {
            let p = Path::new(&project_dir);
            let project_path = if p.is_absolute() {
                p.to_path_buf()
            } else {
                std::env::current_dir()?.join(p)
            };
            claude_config_trust(&project_path);
            Ok(())
        }
    }
}

/// Resolve the effective tokens-pool directory for a `tokens` CLI
/// subcommand's `--workspace` flag (issue #4292, trip-wire 3).
///
/// An **explicit** `--workspace` (anything other than the clap default `"."`)
/// always resolves via today's per-repo/shared precedence
/// ([`resolve_tokens_workspace`] +
/// [`tokens_pool::paths::resolve_tokens_dir`], #3938) — unchanged, so an
/// operator pointing at a specific (possibly unregistered) repo is never
/// silently redirected.
///
/// Only the **default** `"."` case additionally asks whether the resolved cwd
/// is itself a recognized Loom workspace, reusing the exact registry check
/// #4299 established for CLI `--workspace` defaulting
/// ([`workspace_registry::resolve_client_workspace_default`], via
/// [`tokens_pool::paths::resolve_tokens_dir_anchored`]) rather than a second,
/// parallel detection path. When it is not — the machine-level daemon's own
/// `tokens check --ranking` invoked from a bare cwd, or an operator running
/// `loom-daemon tokens check` from `$HOME` — this anchors straight to the
/// shared machine-level pool instead of a per-repo(cwd) path that can
/// coincidentally collide with the shared default (both resolve to
/// `~/.loom/tokens` when cwd is `$HOME`).
///
/// Returns `(tokens_dir, anchored_to_shared)` — the second element is `true`
/// only when the "not a recognized workspace" branch fired, so callers can
/// surface *how* the directory was chosen (not just *which* directory).
fn resolve_tokens_pool_dir_for_cli(workspace: &str) -> Result<(PathBuf, bool)> {
    use loom_daemon::tokens_pool::paths;
    use loom_daemon::workspace_registry::{resolve_client_workspace_default, WorkspaceRegistry};

    let ws = resolve_tokens_workspace(workspace)?;
    if workspace != "." {
        return Ok((paths::resolve_tokens_dir(&ws), false));
    }

    let registry = WorkspaceRegistry::load_default().unwrap_or_default();
    if registry.workspaces.is_empty() {
        return Ok((paths::resolve_tokens_dir(&ws), false));
    }
    // Only actually "anchored to shared" when the shared pool is enabled and
    // would be used — if `LOOM_SHARED_TOKENS_DIR=""` opts out,
    // `resolve_tokens_dir_anchored` falls back to the per-repo(cwd) path
    // instead, and the caller's messaging should say so.
    let anchored_to_shared = resolve_client_workspace_default(&ws, &registry).is_none()
        && paths::shared_tokens_dir().is_some();
    Ok((paths::resolve_tokens_dir_anchored(&ws, &registry), anchored_to_shared))
}

#[cfg(test)]
mod resolve_tokens_workspace_tests {
    //! Tests for [`resolve_tokens_workspace`] (issue #4292). No test here
    //! `chdir`s the process (unsafe to do in a parallel test binary) — the
    //! `"."` case is instead verified against a live `current_dir()` read,
    //! and the absolute-path case needs no cwd at all.
    use super::resolve_tokens_workspace;
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
    use super::resolve_tokens_pool_dir_for_cli;
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

/// Human-readable label for an account's merge provenance. Mirrors
/// `cli._SOURCE_LABEL`.
fn source_label(source: &str) -> &str {
    match source {
        "home" => "home",
        "repo" => "repo",
        "repo-override" => "repo (overrides home)",
        "monitor" => "claude-monitor",
        "monitor-override" => "claude-monitor (overrides repo/home)",
        other => other,
    }
}

/// Print the effective merged account set and where each came from. Mirrors
/// `cli._print_effective_accounts` — secrets are never shown, only email, token
/// filename, and source.
fn print_effective_accounts(result: &loom_daemon::tokens_pool::bootstrap::BootstrapResult) {
    let disp = |p: &Option<std::path::PathBuf>| -> String {
        p.as_ref()
            .map_or_else(|| "(none)".to_string(), |p| p.display().to_string())
    };
    // Resolved absolute pool path (issue #4948, suggested-fix option 3) —
    // surfaced up front so an operator can immediately tell which pool a
    // `bootstrap` run touched (per-repo, `--shared`, or `--workspace`).
    println!("Resolved pool: {}", disp(&result.tokens_dir));
    println!("Account sources:");
    println!("  claude-monitor: {}", disp(&result.monitor_env));
    println!("  home: {}", disp(&result.home_env));
    println!("  repo: {}", disp(&result.repo_env));

    if result.effective.is_empty() {
        println!("Effective accounts: (none)");
        return;
    }

    println!("Effective accounts ({}):", result.effective.len());
    let width = result
        .effective
        .iter()
        .map(|a| a.name.chars().count())
        .max()
        .unwrap_or(0);
    for a in &result.effective {
        let label = source_label(&a.source);
        println!("  {:<width$}  {}  [{label}]", a.name, a.email, width = width);
    }
}

/// Print the imported account set from `import-from-monitor`. Secrets are
/// never shown. Mirrors `cli._print_monitor_import`.
fn print_monitor_import(result: &loom_daemon::tokens_pool::monitor_db::MonitorImportResult) {
    let disp = |p: &Option<std::path::PathBuf>| -> String {
        p.as_ref()
            .map_or_else(|| "(none)".to_string(), |p| p.display().to_string())
    };
    println!("claude-monitor store: {}", disp(&result.db_path));
    println!("Destination pool: {}", disp(&result.tokens_dir));

    if result.effective.is_empty() {
        println!("Active accounts: (none)");
        return;
    }

    let written: std::collections::HashSet<&str> =
        result.written.iter().map(String::as_str).collect();
    let unchanged: std::collections::HashSet<&str> =
        result.unchanged.iter().map(String::as_str).collect();
    let drifted: std::collections::HashSet<&str> =
        result.drifted.iter().map(String::as_str).collect();

    println!("Active accounts ({}):", result.effective.len());
    for a in &result.effective {
        let disposition = if written.contains(a.file.as_str()) {
            "written"
        } else if unchanged.contains(a.file.as_str()) {
            "unchanged"
        } else if drifted.contains(a.file.as_str()) {
            "DRIFT (use --force)"
        } else {
            "-"
        };
        println!("  {}  {}  [{disposition}]", a.name, a.email);
    }

    if !result.pruned.is_empty() {
        println!("Pruned ({}): {}", result.pruned.len(), result.pruned.join(", "));
    }
}

/// Handle `loom-daemon forge <issue|pr|auth|auto-merge>` (epic #4081 Phase 3,
/// family 3 — the native port of `loom-forge` / `loom-auto-merge`). Handlers
/// exec `gh` / exit the process directly, so this only returns `Err` when a
/// child process cannot be spawned. See `loom-daemon/src/forge_cmd.rs`.
pub(crate) fn handle_forge_command(action: ForgeAction) -> Result<()> {
    use loom_daemon::forge_cmd::{dispatch, ForgeCmd};
    let cmd = match action {
        ForgeAction::Issue { args } => ForgeCmd::Issue(args),
        ForgeAction::Pr { args } => ForgeCmd::Pr(args),
        ForgeAction::Auth { args } => ForgeCmd::Auth(args),
        ForgeAction::AutoMerge {
            pr_number,
            method,
            expected_head_sha,
            ..
        } => ForgeCmd::AutoMerge {
            pr: pr_number,
            method,
            expected_head_sha,
        },
    };
    dispatch(cmd)
}

/// Handle `loom-daemon tokens <select|pin|unpin|unblock|mark-bad>` (Issue
/// #4082, Phase 1 of epic #4081; `mark-bad` added in #4228, Phase 2). Purely
/// file-based — does not require a running daemon. See
/// `loom-daemon/src/tokens_pool/mod.rs` for the ported subset and what is
/// deliberately deferred.
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn handle_tokens_command(action: TokensAction) -> Result<()> {
    use loom_daemon::tokens_pool::{allowlist, bad_tokens, failure_counts, select};

    match action {
        TokensAction::Select {
            workspace,
            provider,
            export,
            no_key,
            auto_unpin,
        } => {
            // #5609 (design D8/D9): parsed through `AccountProvider`'s
            // `FromStr` rather than compared against two hardcoded string
            // literals, so the valid vocabulary has exactly one definition
            // and the error message enumerates it from that same place.
            let provider: loom_daemon::tokens_pool::AccountProvider =
                provider.parse().map_err(|error| anyhow!("{error}"))?;
            let ws = resolve_tokens_workspace(&workspace)?;
            // Resolved workspace (issue #4948, suggested-fix option 3) —
            // always stderr so `--export`'s stdout stays eval-safe and
            // `--json`'s (non-`--export`) stdout stays a bare JSON object.
            eprintln!("Resolved workspace: {}", ws.display());
            if provider == loom_daemon::tokens_pool::AccountProvider::Codex {
                let selected = loom_daemon::tokens_pool::select_account(&ws, provider)
                    .map_err(|error| anyhow!(error))?;
                let directory = match &selected.binding {
                    loom_daemon::tokens_pool::AccountBinding::CodexHome { directory } => directory,
                    _ => unreachable!("Codex selection returned a non-Codex binding"),
                };
                if export {
                    println!(
                        "export CODEX_HOME={}",
                        shell_single_quote(&directory.display().to_string())
                    );
                    println!(
                        "export LOOM_ACCOUNT_PROVIDER='{}'\nexport LOOM_ACCOUNT_NAME={}",
                        selected.id.provider,
                        shell_single_quote(&selected.id.name)
                    );
                    // #5609 AC 6: alongside the existing PROVIDER/NAME pair,
                    // so a dispatched sweep can be correlated back to the
                    // exact upstream account. Codex's storage backend has no
                    // upstream id to carry (see `SelectedAccount::upstream_id`
                    // doc comment), so this is a no-op today and becomes live
                    // the day a Codex-backed `ProviderAdapter` gains one.
                    if let Some(upstream_id) = &selected.upstream_id {
                        println!(
                            "export LOOM_ACCOUNT_UPSTREAM_ID={}",
                            shell_single_quote(upstream_id)
                        );
                    }
                    println!("LOOM_TOKEN_MODE='{}'", selected.mode);
                } else {
                    println!(
                        "{}",
                        serde_json::json!({
                            "provider": selected.id.provider.to_string(),
                            "name": selected.id.name,
                            "upstream_id": selected.upstream_id,
                            "credential_kind": "codex_home",
                            "credential_reference": directory,
                            "mode": selected.mode,
                        })
                    );
                }
                return Ok(());
            }
            debug_assert_eq!(provider, loom_daemon::tokens_pool::AccountProvider::Claude);
            if auto_unpin {
                if let Some(msg) = loom_daemon::tokens_pool::maybe_auto_unpin(&ws) {
                    eprintln!("{msg}");
                }
            }
            // Routine `.bad_tokens` hygiene (#4643). `cleanup_bad_tokens` had
            // zero callers in the whole tree, so pools accumulated expired
            // exhaustion entries forever (the live shared pool still held
            // days-old lines). Selection is the routine path every spawn takes,
            // and pruning is behavior-neutral — `is_bad` already ignores
            // aged-out entries — so this only bounds the file. Best-effort:
            // never let a cleanup failure block a spawn, and no lock is taken
            // at all when there is nothing to prune.
            let _ = bad_tokens::cleanup_bad_tokens(&ws, bad_tokens::DEFAULT_CLEANUP_MAX_AGE_SECS);
            match select::select_token(&ws, None) {
                Ok(sel) => {
                    if export {
                        if no_key {
                            println!(
                                "# selected={} mode={} file={}",
                                sel.name,
                                sel.mode,
                                sel.file.display()
                            );
                        } else {
                            // Tokens are base64/hex-like and never contain a
                            // single quote in practice; this is a simple
                            // wrap, not a full Python repr() escape.
                            println!("export CLAUDE_CODE_OAUTH_TOKEN='{}'", sel.key);
                            // Shell-evalable (issue #4228): lets
                            // spawn-claude.sh / claude-wrapper.sh `eval` this
                            // output directly instead of round-tripping
                            // through `python3 -c 'import json...'`.
                            println!("export LOOM_TOKEN_NAME='{}'", sel.name);
                            // #5609 AC 6: alongside the existing
                            // LOOM_TOKEN_NAME, so a dispatched sweep can be
                            // correlated back to the exact upstream account.
                            // `None` for a `.token` file with no `index.json`
                            // row (fail-open pool) — never fabricated.
                            if let Some(upstream_id) = &sel.upstream_id {
                                println!("export LOOM_ACCOUNT_UPSTREAM_ID='{upstream_id}'");
                            }
                            println!("LOOM_TOKEN_MODE='{}'", sel.mode);
                            println!(
                                "# selected={} mode={} file={}",
                                sel.name,
                                sel.mode,
                                sel.file.display()
                            );
                        }
                    } else {
                        let mut obj = serde_json::Map::new();
                        obj.insert("name".to_string(), serde_json::Value::String(sel.name));
                        obj.insert(
                            "provider".to_string(),
                            serde_json::Value::String(provider.to_string()),
                        );
                        obj.insert("upstream_id".to_string(), serde_json::json!(sel.upstream_id));
                        obj.insert(
                            "file".to_string(),
                            serde_json::Value::String(sel.file.display().to_string()),
                        );
                        obj.insert(
                            "mode".to_string(),
                            serde_json::Value::String(sel.mode.to_string()),
                        );
                        if !no_key {
                            obj.insert("key".to_string(), serde_json::Value::String(sel.key));
                        }
                        println!("{}", serde_json::Value::Object(obj));
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(select::EX_CONFIG);
                }
            }
        }

        TokensAction::Bootstrap {
            workspace,
            env,
            home_env,
            no_home,
            shared,
            force,
            dry_run,
            json,
        } => {
            use loom_daemon::tokens_pool::bootstrap::{self, BootstrapError, BootstrapOptions};
            use loom_daemon::tokens_pool::paths::shared_tokens_dir;

            let repo_root = resolve_tokens_workspace(&workspace)?;

            // Refuse when the target workspace declares `daemon.delegatedTo`
            // (issue #5345) — mirrors `workspace add/set-priority/remove`'s
            // gate in `cli::workspace_fleet`. `tokens select` (read-only, on
            // the spawn hot path) is deliberately never gated.
            if let Some(delegate) = loom_daemon::config_resolver::daemon_delegated_to(&repo_root) {
                eprintln!(
                    "error: daemon admin is delegated to {delegate} — perform token bootstrap \
                     there."
                );
                std::process::exit(1);
            }

            // `--shared` redirects the destination pool to the machine-level
            // location (issue #3938). Only the write target changes; account
            // sources are unchanged. Refuse when the shared pool is disabled.
            let tokens_dir = if shared {
                match shared_tokens_dir() {
                    Some(dir) => {
                        eprintln!(
                            "Bootstrapping the shared machine-level pool at {}",
                            dir.display()
                        );
                        Some(dir)
                    }
                    None => {
                        eprintln!(
                            "error: --shared requested but the shared pool is disabled \
                             (LOOM_SHARED_TOKENS_DIR is empty). Unset it or point it at a directory."
                        );
                        std::process::exit(1);
                    }
                }
            } else {
                None
            };

            // `--no-home` disables the master; `--home-env` points elsewhere;
            // neither falls through to default resolution ($LOOM_ACCOUNTS_ENV).
            let home_env_path = if no_home {
                Some(None)
            } else {
                home_env.map(|p| Some(std::path::PathBuf::from(p)))
            };

            let opts = BootstrapOptions {
                repo_root,
                env_path: env.map(std::path::PathBuf::from),
                home_env_path,
                force,
                dry_run,
                tokens_dir,
            };

            let result = match bootstrap::bootstrap_tokens(&opts) {
                Ok(r) => r,
                Err(e @ (BootstrapError::NoSource(_) | BootstrapError::DuplicateFile(_))) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
                Err(BootstrapError::Io(e)) => return Err(anyhow!("bootstrap failed: {e}")),
            };

            // Warnings (partial triples, drift, unreadable tokens) go to stderr
            // so `--json` stdout stays clean.
            for w in &result.warnings {
                eprintln!("warning: {w}");
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&result.to_json())?);
            } else {
                print_effective_accounts(&result);
            }

            // Unresolved drift without --force is a non-zero exit so CI can
            // detect divergence (mirrors cli._cmd_bootstrap).
            if !result.drifted.is_empty() && !force {
                std::process::exit(2);
            }
            Ok(())
        }

        TokensAction::ImportFromMonitor {
            workspace,
            shared,
            db,
            force,
            prune,
            dry_run,
            json,
        } => {
            use loom_daemon::tokens_pool::monitor_db::{
                import_from_monitor, ImportOptions, MonitorImportError,
            };
            use loom_daemon::tokens_pool::paths::shared_tokens_dir;

            // Destination: the shared machine-level pool, or this repo's pool
            // (mirrors cli._cmd_import_from_monitor). `--workspace` is a
            // plain path here (no upward `.git` walk), matching the sibling
            // `bootstrap` / `check` / `select` CLI arms.
            let tokens_dir = if shared {
                match shared_tokens_dir() {
                    Some(dir) => {
                        eprintln!(
                            "Importing into the shared machine-level pool at {}",
                            dir.display()
                        );
                        dir
                    }
                    None => {
                        eprintln!(
                            "error: --shared requested but the shared pool is disabled \
                             (LOOM_SHARED_TOKENS_DIR is empty). Unset it or point it at a directory."
                        );
                        std::process::exit(1);
                    }
                }
            } else {
                let ws = resolve_tokens_workspace(&workspace)?;
                ws.join(".loom").join("tokens")
            };

            let db_path = db.map(std::path::PathBuf::from);
            let opts = ImportOptions {
                tokens_dir: &tokens_dir,
                db_path: db_path.as_deref(),
                monitor_dir: None,
                force,
                dry_run,
                prune,
            };

            let result = match import_from_monitor(&opts) {
                Ok(r) => r,
                Err(
                    e @ (MonitorImportError::DbUnavailable(_)
                    | MonitorImportError::DuplicateFile(_)),
                ) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
                Err(MonitorImportError::Io(e)) => {
                    return Err(anyhow!("import-from-monitor failed: {e}"))
                }
            };

            // Warnings (no active credentials, unreadable tokens, prune
            // failures, drift) go to stderr so `--json` stdout stays clean.
            for w in &result.warnings {
                eprintln!("warning: {w}");
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&result.to_json())?);
            } else {
                print_monitor_import(&result);
            }

            // Unresolved drift without --force is the "rolled tokens not
            // applied" case; exit non-zero so a script notices the pool is
            // still stale (mirrors cli._cmd_import_from_monitor).
            if !result.drifted.is_empty() && !force {
                std::process::exit(2);
            }
            Ok(())
        }

        TokensAction::Check {
            workspace,
            ranking,
            source,
            probe_prompt,
            json,
            no_stagger,
        } => {
            use loom_daemon::tokens_pool::check::{
                self, CheckOptions, CurlTransport, DEFAULT_PROBE_MODEL, DEFAULT_PROBE_PROMPT,
            };

            let (tokens_dir, anchored_to_shared) = resolve_tokens_pool_dir_for_cli(&workspace)?;
            // Resolved absolute pool path (issue #4948, suggested-fix option
            // 3) — `import-from-monitor` already prints its `Destination
            // pool:` line; `check` did not until now, which is exactly how
            // the reported nested-pool incident went unnoticed (the command
            // reported success against a pool the operator never saw named).
            // Always goes to stderr, unconditional of `--json`, so scripted
            // callers' stdout stays parseable.
            eprintln!("Resolved pool: {}", tokens_dir.display());
            if anchored_to_shared {
                eprintln!(
                    "note: --workspace defaulted to a directory that is not a registered Loom \
                     workspace; anchoring to the shared machine-level pool at {} (issue #4292)",
                    tokens_dir.display()
                );
            }

            // Routine `.bad_tokens` hygiene (#4643) — the second wired path.
            // `tokens check` is the low-frequency periodic probe (the daemon's
            // ranking refresh runs it on a cadence), so it prunes the pool the
            // check is actually anchored to, which may be the shared
            // machine-level pool rather than `resolve_tokens_dir(workspace)`.
            match bad_tokens::cleanup_bad_tokens_in_dir(
                &tokens_dir,
                bad_tokens::DEFAULT_CLEANUP_MAX_AGE_SECS,
            ) {
                Ok(outcome) if outcome.removed > 0 => eprintln!(
                    "note: pruned {} expired .bad_tokens entr{} older than {}h ({} retained)",
                    outcome.removed,
                    if outcome.removed == 1 { "y" } else { "ies" },
                    bad_tokens::DEFAULT_CLEANUP_MAX_AGE_SECS / 3600,
                    outcome.kept,
                ),
                Ok(_) => {}
                Err(e) => eprintln!("warning: .bad_tokens cleanup skipped: {e}"),
            }

            let source_flag = match source {
                Some(raw) => match check::Source::parse(&raw) {
                    Some(s) => Some(s),
                    None => {
                        eprintln!(
                            "error: invalid --source {raw:?}; expected one of auto, monitor, probe"
                        );
                        std::process::exit(2);
                    }
                },
                None => None,
            };
            let resolved_source = check::resolve_source(source_flag);
            let prompt = probe_prompt.unwrap_or_else(|| DEFAULT_PROBE_PROMPT.to_string());

            let opts = CheckOptions {
                source: resolved_source,
                write_ranking: ranking,
                probe_prompt: &prompt,
                model: DEFAULT_PROBE_MODEL,
                stagger: !no_stagger,
            };
            let transport = CurlTransport;
            let report = check::run_check(&tokens_dir, &opts, &transport);

            if json {
                println!("{}", serde_json::to_string_pretty(&report.to_json())?);
            } else {
                println!("{}", check::format_table(&report));
            }

            // Exit 1 only when every probe failed (selector has nothing usable).
            if !report.accounts.is_empty()
                && report
                    .accounts
                    .iter()
                    .all(|a| a.status == "error" || a.status == "skipped")
            {
                std::process::exit(1);
            }
            Ok(())
        }

        TokensAction::Pin { action, workspace } => {
            let ws = resolve_tokens_workspace(&workspace)?;
            handle_pin_action(action, &ws)
        }

        TokensAction::Unpin { workspace, json } => {
            let ws = resolve_tokens_workspace(&workspace)?;
            let had_file = allowlist::clear_allowlist(&ws).map_err(|e| anyhow!(e))?;
            let _ = failure_counts::reset_all(&ws);
            if json {
                println!("{}", serde_json::json!({ "cleared": had_file }));
            } else if had_file {
                println!("Allowlist cleared. All accounts are eligible.");
            } else {
                println!("No allowlist was active.");
            }
            Ok(())
        }

        TokensAction::Unblock {
            names,
            workspace,
            all_reasons,
            shared,
            json,
        } => {
            use loom_daemon::tokens_pool::paths::shared_tokens_dir;

            // Resolve the target pool directory once, up front. `--shared`
            // redirects straight to the machine-level pool (parity with
            // `bootstrap --shared` / `import-from-monitor --shared`, issue
            // #6759) — the *destination* changes, account sources do not.
            let dir = if shared {
                match shared_tokens_dir() {
                    Some(dir) => {
                        eprintln!(
                            "Unblocking against the shared machine-level pool at {}",
                            dir.display()
                        );
                        dir
                    }
                    None => {
                        eprintln!(
                            "error: --shared requested but the shared pool is disabled \
                             (LOOM_SHARED_TOKENS_DIR is empty). Unset it or point it at a directory."
                        );
                        std::process::exit(1);
                    }
                }
            } else {
                let ws = resolve_tokens_workspace(&workspace)?;
                loom_daemon::tokens_pool::paths::resolve_tokens_dir(&ws)
            };

            let available = allowlist::list_accounts_in_dir(&dir);
            let available_set: std::collections::HashSet<&str> =
                available.iter().map(String::as_str).collect();

            // A name is recognized either because it is in the live pool, or
            // because it already has a `.bad_tokens` entry (issue #6759) —
            // the latter is exactly the case of a retired account whose
            // stale entry can otherwise never be cleared through any
            // supported path. Only names matching neither are truly unknown;
            // those are reported and skipped rather than aborting the whole
            // batch (previously `std::process::exit(2)` before any name was
            // processed).
            let mut validated: Vec<String> = Vec::new();
            let mut unknown: Vec<String> = Vec::new();
            for raw in &names {
                let name = raw.trim();
                if name.is_empty() {
                    continue;
                }
                if available_set.contains(name)
                    || bad_tokens::blocking_entry_in_dir(&dir, name).is_some()
                {
                    validated.push(name.to_string());
                } else {
                    unknown.push(name.to_string());
                }
            }
            if !unknown.is_empty() {
                let avail = if available.is_empty() {
                    "(none)".to_string()
                } else {
                    available.join(", ")
                };
                let plural = if unknown.len() == 1 { "" } else { "s" };
                eprintln!(
                    "Unknown account{plural} (not in the pool and no `.bad_tokens` entry), \
                     skipping: {}. Available: {avail}",
                    unknown.join(", ")
                );
            }
            if validated.is_empty() {
                eprintln!("`unblock` requires at least one recognized account name.");
                std::process::exit(if unknown.is_empty() { 1 } else { 2 });
            }

            let outcome = bad_tokens::unblock_in_dir(&dir, &validated, all_reasons)
                .map_err(|e| anyhow!(e))?;
            let removed = outcome.removed;
            let kept = outcome.kept;
            let excluded = outcome.excluded;
            for name in &validated {
                let _ = failure_counts::record_success_in_dir(&dir, name);
            }

            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "removed": removed,
                        "kept": kept,
                        "excluded": excluded,
                        "unknown": unknown,
                    })
                );
            } else {
                if removed > 0 {
                    let plural = if removed == 1 { "y" } else { "ies" };
                    println!(
                        "Removed {removed} bad-token entr{plural} for: {}",
                        validated.join(", ")
                    );
                }
                if !excluded.is_empty() {
                    // #4212: a no-op that looks like success is the failure
                    // mode. Name the still-blocked accounts and fail below.
                    let plural = if excluded.len() == 1 { "y" } else { "ies" };
                    eprintln!(
                        "Left {} non-auth (exhausted/rate-limited) entr{plural} in place for: \
                         {}. These are still blocking selection — re-run with --all-reasons to \
                         drop them (or wait for the exhaustion cooldown to expire them \
                         automatically).",
                        excluded.len(),
                        excluded.join(", ")
                    );
                } else if removed == 0 {
                    println!(
                        "No matching entries removed (use --all-reasons to drop non-auth entries \
                         too)."
                    );
                }
            }

            // Non-zero when the default scope left the named accounts still
            // blocked — the operator's intent ("unblock X") was not achieved.
            if !excluded.is_empty() {
                std::process::exit(3);
            }
            Ok(())
        }

        TokensAction::MarkBad {
            name,
            reason,
            workspace,
            json,
        } => {
            let ws = resolve_tokens_workspace(&workspace)?;
            match bad_tokens::mark_bad(&ws, &name, &reason) {
                Ok(()) => {
                    if json {
                        println!("{}", serde_json::json!({ "marked": true, "name": name }));
                    } else {
                        println!("Marked '{name}' bad ({reason}).");
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}

/// Handle `loom-daemon tokens pin <set|add|remove|status>`.
fn handle_pin_action(action: PinAction, ws: &Path) -> Result<()> {
    use loom_daemon::tokens_pool::{allowlist, failure_counts};

    match action {
        PinAction::Set { names } => match allowlist::write_allowlist(ws, &names) {
            Ok(written) => {
                let _ = failure_counts::reset_all(ws);
                if written.is_empty() {
                    println!("Allowlist cleared (no names resolved). All accounts are eligible.");
                } else {
                    println!(
                        "Allowlist set to {} account(s): {}",
                        written.len(),
                        written.join(", ")
                    );
                }
                Ok(())
            }
            Err(allowlist::AllowlistError::Unknown(e)) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
            Err(allowlist::AllowlistError::Io(e)) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },

        PinAction::Add { names } => match allowlist::add_to_allowlist(ws, &names) {
            Ok((added, skipped)) => {
                let _ = failure_counts::reset_all(ws);
                if !added.is_empty() {
                    println!("Added {} account(s): {}", added.len(), added.join(", "));
                }
                if !skipped.is_empty() {
                    println!("Already present: {}", skipped.join(", "));
                }
                Ok(())
            }
            Err(allowlist::AllowlistError::Unknown(e)) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
            Err(allowlist::AllowlistError::Io(e)) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },

        PinAction::Remove { names } => match allowlist::remove_from_allowlist(ws, &names) {
            Ok((removed, skipped)) => {
                let _ = failure_counts::reset_all(ws);
                if !removed.is_empty() {
                    println!("Removed {} account(s): {}", removed.len(), removed.join(", "));
                }
                if !skipped.is_empty() {
                    println!("Not in allowlist: {}", skipped.join(", "));
                }
                Ok(())
            }
            Err(allowlist::AllowlistError::Unknown(e)) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
            Err(allowlist::AllowlistError::Io(e)) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },

        PinAction::Status { json } => {
            let all_accounts = allowlist::list_accounts(ws);
            let active = allowlist::read_allowlist(ws);
            if json {
                let payload = serde_json::json!({
                    "allowlist_active": !active.is_empty(),
                    "allowlist": active,
                    "accounts": all_accounts,
                });
                println!("{payload}");
                return Ok(());
            }
            if active.is_empty() {
                println!("No allowlist active — all accounts are eligible.");
            } else {
                println!("Allowlist active ({} account(s)):", active.len());
                for name in &active {
                    println!("  * {name}");
                }
            }
            if all_accounts.is_empty() {
                println!();
                eprintln!("No .token files found. Run `loom-daemon tokens bootstrap` first.");
            } else {
                println!();
                println!("Available accounts ({}):", all_accounts.len());
                let active_set: std::collections::HashSet<&str> =
                    active.iter().map(String::as_str).collect();
                for name in &all_accounts {
                    let mark = if active_set.contains(name.as_str()) {
                        "*"
                    } else {
                        " "
                    };
                    println!("  {mark} {name}");
                }
            }
            Ok(())
        }
    }
}
