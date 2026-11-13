//! Per-agent `CLAUDE_CONFIG_DIR` isolation.
//!
//! Creates isolated Claude Code config directories for each terminal so concurrent
//! terminals don't fight over sessions, lock files, and temp directories in the
//! shared `~/.claude/` directory.
//!
//! Originally a port of `loom_tools.common.claude_config`; since epic #4081
//! Phase 3 family 4 (#4415) deleted that module, this is the **sole**
//! implementation. Reached three ways: the daemon's
//! `create_terminal`/`destroy_terminal` ([`crate::terminal::TerminalManager`]),
//! the native `agent-spawn` path (`agent_session::SystemEnv`), and the
//! standalone `loom-daemon claude-config` CLI. The `_SHARED_CONFIG_FILES` /
//! `_MUTABLE_DIRS` comments below still name their historical Python
//! counterparts for provenance.
//!
//! This module lived inline inside `terminal.rs` until issue #13857, reached
//! from outside through a block of `pub fn claude_config_*` forwarding
//! wrappers. It is a crate-level module now, so every entry point is called
//! directly as `crate::claude_config::<fn>` and a new one needs no wrapper.

mod keychain_write;

use keychain_write::{keychain_write_command, run_keychain_write};
use std::fs;
use std::path::{Path, PathBuf};

/// Shared config files to symlink from ~/.claude/ (read-only).
/// Must match the Python `_SHARED_CONFIG_FILES` list.
/// NOTE: .claude.json is NOT here — it lives at ~/.claude.json (home root),
/// not inside ~/.claude/. It's handled separately via `resolve_state_file()`.
/// NOTE: settings.json is intentionally excluded — it is copied and filtered
/// to strip `enabledPlugins` (global MCP plugins cause ghost sessions, #2799).
const SHARED_CONFIG_FILES: &[&str] = &["config.json"];

/// Shared directories to symlink from ~/.claude/ (read-only caches).
/// Must match the Python `_SHARED_CONFIG_DIRS` list.
const SHARED_CONFIG_DIRS: &[&str] = &["statsig"];

/// Mutable directories that each agent needs its own copy of.
/// Must match the Python `_MUTABLE_DIRS` list.
const MUTABLE_DIRS: &[&str] = &[
    "projects",
    "todos",
    "debug",
    "file-history",
    "session-env",
    "tasks",
    "plans",
    "shell-snapshots",
    "tmp",
];

/// Project-scoped subdirectories of `<repo_root>/.claude/` that must be
/// visible inside `CLAUDE_CONFIG_DIR` for Claude Code 2.1+ to resolve
/// namespaced slash commands (`/loom:<role>`) and skill routing.
///
/// Must match the Python `_PROJECT_CLAUDE_LINKS` list.  See issue #3346.
const PROJECT_CLAUDE_LINKS: &[&str] = &["commands", "agents"];

/// Create an isolated `CLAUDE_CONFIG_DIR` for a terminal.
///
/// Creates `.loom/claude-config/{agent_name}/` with symlinks to shared
/// read-only config from `~/.claude/` and fresh directories for mutable state.
///
/// Idempotent — safe to call multiple times.
/// Resolve the Claude Code state file path.
///
/// Resolution order:
/// 1. ~/.claude/.config.json  (if it exists)
/// 2. ~/.claude.json          (fallback, most common)
fn resolve_state_file(home: &Path) -> PathBuf {
    let preferred = home.join(".claude").join(".config.json");
    if preferred.exists() {
        return preferred;
    }
    home.join(".claude.json")
}

/// Build the keychain service name Claude Code uses for a config dir.
///
/// Claude Code v2.1.42+ appends a SHA-256 hash of the config dir path
/// to the keychain service name when `CLAUDE_CONFIG_DIR` is set.
fn keychain_service_name(config_dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(config_dir.to_string_lossy().as_bytes());
    let hash = hex::encode(hasher.finalize());
    format!("Claude Code-credentials-{}", &hash[..8])
}

/// Clone macOS Keychain credentials to the per-config-dir service name.
fn clone_keychain_credentials(config_dir: &Path) {
    let account = std::env::var("USER").unwrap_or_else(|_| "claude-code-user".to_string());
    let target_service = keychain_service_name(config_dir);

    // Always re-clone so an expired token in the hashed entry gets refreshed.
    // The write command uses -U (update-or-insert) so this is safe to run on
    // every agent startup.

    // Read the default credential
    let read = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-a",
            &account,
            "-w",
            "-s",
            "Claude Code-credentials",
        ])
        .output();
    let cred = match read {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => {
            log::debug!("No default Claude Code keychain credential found");
            return;
        }
    };

    if cred.is_empty() {
        return;
    }

    let Some(write) = keychain_write_command(&account, &target_service, &cred) else {
        log::warn!(
            "Keychain credential too large to clone to {target_service} via `security -i`; skipping"
        );
        return;
    };

    if run_keychain_write(&write) {
        log::debug!("Cloned keychain credential to {target_service}");
    } else {
        log::warn!("Failed to clone keychain credential to {target_service}");
    }
}

/// Ensure `.claude.json` has the fields required to skip the onboarding wizard.
///
/// Claude Code requires both `hasCompletedOnboarding = true` and a truthy
/// `theme` value to bypass the first-run wizard.  If the state file is
/// missing, dangling (broken symlink), or doesn't contain these fields, we
/// merge the required fields into the existing data (preserving all other
/// fields) rather than replacing the entire file.
fn ensure_onboarding_complete(state_path: &Path) {
    // Try to read existing data (resolving symlinks).
    let mut existing_data = serde_json::Map::new();
    if state_path.exists() {
        if let Ok(contents) = fs::read_to_string(state_path) {
            if let Ok(serde_json::Value::Object(map)) =
                serde_json::from_str::<serde_json::Value>(&contents)
            {
                // Check if all required fields are already present.
                let has_onboarding =
                    map.get("hasCompletedOnboarding") == Some(&serde_json::Value::Bool(true));
                let has_theme = map
                    .get("theme")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|s| !s.is_empty());
                let has_effort =
                    map.get("effortCalloutDismissed") == Some(&serde_json::Value::Bool(true));
                let has_opus =
                    map.get("opusProMigrationComplete") == Some(&serde_json::Value::Bool(true));
                if has_onboarding && has_theme && has_effort && has_opus {
                    return; // All required fields present
                }
                existing_data = map;
            }
        }
    }

    // Merge: fill in only the missing required fields, preserving everything else.
    existing_data
        .entry("hasCompletedOnboarding")
        .or_insert(serde_json::Value::Bool(true));
    if existing_data
        .get("theme")
        .and_then(serde_json::Value::as_str)
        .is_none_or(str::is_empty)
    {
        existing_data.insert("theme".to_string(), serde_json::Value::String("dark".to_string()));
    }
    existing_data
        .entry("effortCalloutDismissed")
        .or_insert(serde_json::Value::Bool(true));
    existing_data
        .entry("opusProMigrationComplete")
        .or_insert(serde_json::Value::Bool(true));

    // Remove whatever is there (dangling symlink, corrupt file, etc.)
    // so we can write a standalone file.
    let _ = fs::remove_file(state_path);

    let merged = serde_json::Value::Object(existing_data);
    if let Err(e) = fs::write(state_path, merged.to_string()) {
        log::warn!("Failed to write merged .claude.json: {e}");
    } else {
        log::debug!("Wrote merged .claude.json with onboarding-complete state");
    }
}

/// Copy `settings.json` stripping the `enabledPlugins` key.
///
/// Global MCP plugins (e.g. rust-analyzer-lsp, swift-lsp) load from the
/// `enabledPlugins` field in `~/.claude/settings.json`.  In headless agent
/// sessions these plugins fail to initialise and can prevent Claude CLI
/// from processing its input prompt, producing ghost sessions that waste
/// minutes of retry time.  See issue #2799.
fn copy_settings_without_plugins(src: &Path, dst: &Path) -> bool {
    let content = match fs::read_to_string(src) {
        Ok(c) => c,
        Err(e) => {
            log::debug!("Could not read {}: {e} — skipping settings copy", src.display());
            return false;
        }
    };

    let mut data: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            log::debug!("settings.json is not valid JSON: {e} — skipping");
            return false;
        }
    };

    if let Some(obj) = data.as_object_mut() {
        obj.remove("enabledPlugins");
    } else {
        log::debug!("settings.json is not a JSON object — skipping");
        return false;
    }

    match fs::write(dst, serde_json::to_string_pretty(&data).unwrap_or_default()) {
        Ok(()) => {
            log::debug!("Copied settings.json to {} (enabledPlugins stripped)", dst.display());
            true
        }
        Err(e) => {
            log::debug!("Failed to write filtered settings.json to {}: {e}", dst.display());
            false
        }
    }
}

/// Refresh symlinks from `CLAUDE_CONFIG_DIR/<name>` to
/// `<repo>/.claude/<name>` for each entry in `PROJECT_CLAUDE_LINKS`.
///
/// This is what makes the per-agent `CLAUDE_CONFIG_DIR` resolve
/// project-defined namespaced slash commands such as `/loom:shepherd`
/// (issue #3346).
///
/// Behaviour:
///   - Source missing  → skip silently (nothing to link).
///   - Destination missing  → create the symlink.
///   - Destination is a symlink to the correct target  → no-op.
///   - Destination is a symlink to a different path (e.g. moved worktree)
///     → replace it.
///   - Destination is a regular file or directory  → leave alone (warn).
fn link_project_claude_dirs(repo_root: &Path, config_dir: &Path) {
    let project_claude = repo_root.join(".claude");
    for name in PROJECT_CLAUDE_LINKS {
        let src = project_claude.join(name);
        if !src.exists() {
            // No project-side dir to link — harmless for repos that
            // haven't installed Loom's command tree.
            continue;
        }
        // Use the canonical absolute path so the link survives `cd`/cwd
        // changes and is unambiguous when inspected with `ls -l`.
        let src_abs = match fs::canonicalize(&src) {
            Ok(p) => p,
            Err(e) => {
                log::debug!("Could not canonicalize project link source {}: {e}", src.display());
                continue;
            }
        };

        let dst = config_dir.join(name);
        let dst_meta = fs::symlink_metadata(&dst);
        match dst_meta {
            Ok(meta) if meta.file_type().is_symlink() => {
                // Already a symlink — check whether it points at the
                // correct target.  Resolve via canonicalize so we
                // compare absolute paths.
                if let Ok(cur) = fs::canonicalize(&dst) {
                    if cur == src_abs {
                        continue;
                    }
                }
                // Wrong target (or unreadable) — refresh.
                if let Err(e) = fs::remove_file(&dst) {
                    log::debug!("Could not unlink stale symlink {}: {e}", dst.display());
                    continue;
                }
            }
            Ok(_) => {
                // Non-symlink directory or file present — don't destroy
                // it.  Warn and skip; commands won't resolve via this
                // config dir, but we won't lose user data.
                log::warn!(
                    "Skipping project-claude link for {name}: {} exists and is not a symlink",
                    dst.display()
                );
                continue;
            }
            Err(_) => {
                // Doesn't exist — fall through to create.
            }
        }

        if let Err(e) = std::os::unix::fs::symlink(&src_abs, &dst) {
            log::warn!("Failed to symlink {} -> {}: {e}", dst.display(), src_abs.display());
        } else {
            log::debug!("Linked {} -> {}", dst.display(), src_abs.display());
        }
    }
}

pub fn setup_agent_config_dir(agent_name: &str, repo_root: &Path) -> Option<PathBuf> {
    let config_dir = repo_root
        .join(".loom")
        .join("claude-config")
        .join(agent_name);

    if let Err(e) = fs::create_dir_all(&config_dir) {
        log::warn!("Failed to create agent config dir {}: {e}", config_dir.display());
        return None;
    }

    let Some(home) = dirs::home_dir() else {
        log::warn!("Could not determine home directory for CLAUDE_CONFIG_DIR setup");
        return Some(config_dir);
    };
    let home_claude = home.join(".claude");

    // Symlink shared config files from ~/.claude/
    for filename in SHARED_CONFIG_FILES {
        let src = home_claude.join(filename);
        let dst = config_dir.join(filename);
        if src.exists() && !dst.exists() {
            if let Err(e) = std::os::unix::fs::symlink(&src, &dst) {
                log::debug!("Failed to symlink {}: {e}", dst.display());
            }
        }
    }

    // Copy settings.json with enabledPlugins stripped (issue #2799).
    // Global plugins (rust-analyzer-lsp, swift-lsp, etc.) fail in headless
    // mode and cause ghost sessions.  All other settings are preserved.
    let settings_dst = config_dir.join("settings.json");
    if !settings_dst.exists() {
        let settings_src = home_claude.join("settings.json");
        let copied = if settings_src.exists() {
            copy_settings_without_plugins(&settings_src, &settings_dst)
        } else {
            false
        };
        if !copied && !settings_dst.exists() {
            // The copy failed or source was missing.  Write a minimal
            // fallback so Claude Code never falls back to the global
            // ~/.claude/settings.json which may contain enabledPlugins.
            // See issue #3065.
            if settings_src.is_file() {
                log::warn!(
                    "Failed to copy settings.json from {} — writing minimal \
                     fallback to prevent enabledPlugins leak",
                    settings_src.display()
                );
            }
            if let Err(e) = fs::write(&settings_dst, "{}\n") {
                log::warn!("Failed to write fallback settings.json: {e}");
            }
        }
    }

    // Symlink Claude Code state file (onboarding completion, theme, etc.).
    // The state file lives at ~/.claude.json (or ~/.claude/.config.json),
    // NOT inside ~/.claude/. When CLAUDE_CONFIG_DIR is overridden, Claude
    // looks for $CLAUDE_CONFIG_DIR/.claude.json.
    let state_src = resolve_state_file(&home);
    let state_dst = config_dir.join(".claude.json");
    if state_src.exists() && !state_dst.exists() {
        if let Err(e) = std::os::unix::fs::symlink(&state_src, &state_dst) {
            log::debug!("Failed to symlink state file: {e}");
        }
    }

    // Fallback: ensure the state file has onboarding-complete fields.
    // If the symlink wasn't created (source missing), is dangling, or the
    // target doesn't contain the required fields, write a standalone file.
    ensure_onboarding_complete(&state_dst);

    // Symlink shared directories
    for dirname in SHARED_CONFIG_DIRS {
        let src = home_claude.join(dirname);
        let dst = config_dir.join(dirname);
        if src.exists() && !dst.exists() {
            if let Err(e) = std::os::unix::fs::symlink(&src, &dst) {
                log::debug!("Failed to symlink dir {}: {e}", dst.display());
            }
        }
    }

    // Create mutable directories
    for dirname in MUTABLE_DIRS {
        let dir = config_dir.join(dirname);
        if let Err(e) = fs::create_dir_all(&dir) {
            log::debug!("Failed to create mutable dir {}: {e}", dir.display());
        }
    }

    // Symlink project's `.claude/commands` and `.claude/agents` into
    // the per-agent `CLAUDE_CONFIG_DIR` so Claude Code 2.1+ can resolve
    // namespaced slash commands like `/loom:shepherd`.  Without these
    // links the config dir is a bare directory and Claude Code falls
    // back to "Unknown command" even when `.claude/commands/loom/
    // <role>.md` exists in the project.  See issue #3346.
    link_project_claude_dirs(repo_root, &config_dir);

    // Clone macOS Keychain credentials to the per-config-dir service name.
    clone_keychain_credentials(&config_dir);

    Some(config_dir)
}

/// Remove one agent's config directory.
pub fn cleanup_agent_config_dir(agent_name: &str, repo_root: &Path) -> bool {
    let config_dir = repo_root
        .join(".loom")
        .join("claude-config")
        .join(agent_name);

    if config_dir.is_dir() {
        if let Err(e) = fs::remove_dir_all(&config_dir) {
            log::warn!("Failed to remove agent config dir {}: {e}", config_dir.display());
            return false;
        }
        true
    } else {
        false
    }
}

/// Validate that an agent's config directory is in a healthy state.
///
/// Checks that the config directory exists and its key components are
/// intact: `.claude.json` is a valid symlink (pointing to an existing
/// target) or a valid plain file — but NOT a broken/dangling symlink —
/// `settings.json` exists, all mutable directories exist, and any
/// project-claude link whose source is present resolves.  Used before
/// each spawn/retry attempt to detect corrupted state that
/// `setup_agent_config_dir` (idempotent, skips existing files) would
/// otherwise silently preserve. Mirrors the historical Python
/// `validate_agent_config_dir`. See issue #2909.
pub fn validate_agent_config_dir(agent_name: &str, repo_root: &Path) -> bool {
    let config_dir = repo_root
        .join(".loom")
        .join("claude-config")
        .join(agent_name);
    if !config_dir.is_dir() {
        return false; // Missing — not an error, just not set up yet.
    }

    // `.claude.json`: if it's a symlink, the target must exist (not
    // dangling); if it's a plain file, it must exist.
    let state_dst = config_dir.join(".claude.json");
    let is_symlink = fs::symlink_metadata(&state_dst)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if is_symlink {
        if !state_dst.exists() {
            log::debug!(
                "Agent config dir validation failed for {agent_name}: \
                 .claude.json is a dangling symlink (target missing)"
            );
            return false;
        }
    } else if !state_dst.exists() {
        log::debug!("Agent config dir validation failed for {agent_name}: .claude.json is missing");
        return false;
    }

    // settings.json must exist — without it Claude Code falls back to
    // the global ~/.claude/settings.json which may contain
    // enabledPlugins, causing ghost sessions (issue #3065).
    let settings_file = config_dir.join("settings.json");
    if !settings_file.is_file() {
        log::debug!(
            "Agent config dir validation failed for {agent_name}: settings.json is missing"
        );
        return false;
    }

    for dirname in MUTABLE_DIRS {
        if !config_dir.join(dirname).is_dir() {
            log::debug!(
                "Agent config dir validation failed for {agent_name}: mutable dir '{dirname}' is missing"
            );
            return false;
        }
    }

    // Project-claude links must be present and resolvable whenever the
    // project itself has the corresponding directory.  A missing or
    // dangling link here causes Claude Code 2.1+ to fail with "Unknown
    // command: /loom:<role>". See issue #3346.
    let project_claude = repo_root.join(".claude");
    for name in PROJECT_CLAUDE_LINKS {
        let src = project_claude.join(name);
        if !src.exists() {
            continue; // Project doesn't have this dir — nothing to validate.
        }
        let dst = config_dir.join(name);
        if !dst.exists() {
            log::debug!(
                "Agent config dir validation failed for {agent_name}: project link '{name}' is missing or dangling"
            );
            return false;
        }
    }

    true
}

/// Resolve the Claude Code state file path relative to the current
/// user's home directory. CLI-facing convenience wrapper around
/// `resolve_state_file`, which takes an explicit `home` for testability.
pub fn default_state_file() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    resolve_state_file(&home)
}

/// Ensure `.claude.json` marks `project_dir` as trusted (skip the
/// folder-trust modal).
///
/// Claude Code shows a blocking "Is this a project you created or one
/// you trust?" modal on first launch in any working directory that has
/// no `projects[<abs-path>].hasTrustDialogAccepted: true` entry in the
/// state file. In a non-interactive agent spawn into a freshly-created
/// repo/worktree that Claude Code has never opened, the role command is
/// delivered as keystrokes into that modal instead of being run,
/// stalling or killing the session. See issue #4334.
///
/// Behavioral contract:
/// - **Merge, never replace**: only the `projects[<project_dir>]`
///   entry's `hasTrustDialogAccepted` flag is touched; every other
///   field (and every other `projects` entry) is preserved unchanged.
/// - **Write through the symlink** (issue #2835): when `state_path` is
///   a healthy symlink, write to the resolved target rather than
///   unlinking and replacing it with a standalone file, which would
///   sever every future session from the operator's global state.
/// - Gated by `LOOM_AUTO_TRUST` (default enabled; set to "0" to disable).
/// - Idempotent: a no-op (no rewrite) if already trusted.
pub fn ensure_project_trusted(state_path: &Path, project_dir: &Path) {
    if std::env::var("LOOM_AUTO_TRUST").as_deref() == Ok("0") {
        log::debug!("Project trust auto-seed disabled via LOOM_AUTO_TRUST=0");
        return;
    }

    let project_key = project_dir.to_string_lossy().to_string();

    let mut existing_data = serde_json::Map::new();
    if state_path.exists() {
        if let Ok(contents) = fs::read_to_string(state_path) {
            if let Ok(serde_json::Value::Object(map)) =
                serde_json::from_str::<serde_json::Value>(&contents)
            {
                existing_data = map;
            }
        }
    }

    let mut projects = existing_data
        .get("projects")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    if let Some(entry) = projects.get(&project_key).and_then(|v| v.as_object()) {
        if entry.get("hasTrustDialogAccepted") == Some(&serde_json::Value::Bool(true)) {
            return; // Already trusted — idempotent no-op, no rewrite.
        }
    }

    let mut project_entry = projects
        .get(&project_key)
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    project_entry.insert("hasTrustDialogAccepted".to_string(), serde_json::Value::Bool(true));
    projects.insert(project_key.clone(), serde_json::Value::Object(project_entry));
    existing_data.insert("projects".to_string(), serde_json::Value::Object(projects));
    let merged = serde_json::Value::Object(existing_data);

    // Write through a healthy symlink rather than replacing it (issue #2835).
    if let Ok(meta) = fs::symlink_metadata(state_path) {
        if meta.file_type().is_symlink() {
            if let Ok(target) = fs::canonicalize(state_path) {
                if target.exists() {
                    if let Err(e) = fs::write(&target, merged.to_string()) {
                        log::warn!("Failed to write trust state to symlink target: {e}");
                    } else {
                        log::debug!(
                            "Updated symlink target .claude.json with trust for {project_key}"
                        );
                    }
                    return;
                }
            }
            // Dangling symlink — fall through to unlink and recreate below.
        }
    }

    let _ = fs::remove_file(state_path);
    if let Err(e) = fs::write(state_path, merged.to_string()) {
        log::warn!("Failed to write merged .claude.json with trust: {e}");
    } else {
        log::debug!("Wrote merged .claude.json with trust for {project_key}");
    }
}

/// Mark `project_dir` as trusted in the resolved Claude Code state file, so
/// a non-interactive spawn into it never stalls on the folder-trust modal.
///
/// The composing entry point: resolves the real state file via
/// [`default_state_file`] and hands it to [`ensure_project_trusted`]. Callers
/// that already hold a state path (or want a scratch one in a test) should
/// call [`ensure_project_trusted`] directly instead.
pub fn trust_project(project_dir: &Path) {
    ensure_project_trusted(&default_state_file(), project_dir);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_setup_creates_config_dir_and_mutable_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();

        // Create .loom directory so the path is valid
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        let result = setup_agent_config_dir("terminal-1", repo_root);
        assert!(result.is_some());

        let config_dir = result.unwrap();
        assert!(config_dir.is_dir());
        assert_eq!(config_dir, repo_root.join(".loom/claude-config/terminal-1"));

        // Verify mutable directories were created
        for dirname in MUTABLE_DIRS {
            assert!(config_dir.join(dirname).is_dir(), "Mutable dir '{dirname}' should exist");
        }
    }

    #[test]
    fn test_setup_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        let first = setup_agent_config_dir("terminal-2", repo_root);
        let second = setup_agent_config_dir("terminal-2", repo_root);

        assert_eq!(first, second);
        assert!(first.unwrap().is_dir());
    }

    #[test]
    fn test_setup_creates_symlinks_to_home_claude() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        let result = setup_agent_config_dir("terminal-3", repo_root);
        assert!(result.is_some());

        let config_dir = result.unwrap();

        // Check that symlinks were created for files that exist in ~/.claude/
        let home_claude = dirs::home_dir().unwrap().join(".claude");
        for filename in SHARED_CONFIG_FILES {
            let src = home_claude.join(filename);
            let dst = config_dir.join(filename);
            if src.exists() {
                assert!(dst.symlink_metadata().is_ok(), "Symlink should exist for {filename}");
            }
        }

        for dirname in SHARED_CONFIG_DIRS {
            let src = home_claude.join(dirname);
            let dst = config_dir.join(dirname);
            if src.exists() {
                assert!(dst.symlink_metadata().is_ok(), "Symlink should exist for dir {dirname}");
            }
        }
    }

    #[test]
    fn test_setup_skips_missing_home_claude_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        // This should not panic even if ~/.claude/ files are missing
        let result = setup_agent_config_dir("terminal-4", repo_root);
        assert!(result.is_some());
    }

    #[test]
    fn test_cleanup_removes_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        // Set up first
        let config_dir = setup_agent_config_dir("terminal-5", repo_root).unwrap();
        assert!(config_dir.is_dir());

        // Cleanup
        let removed = cleanup_agent_config_dir("terminal-5", repo_root);
        assert!(removed);
        assert!(!config_dir.exists());
    }

    #[test]
    fn test_cleanup_returns_false_for_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        let removed = cleanup_agent_config_dir("nonexistent", repo_root);
        assert!(!removed);
    }

    #[test]
    fn test_ensure_onboarding_writes_fallback_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join(".claude.json");
        ensure_onboarding_complete(&state);
        assert!(state.exists());
        let data: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(data["hasCompletedOnboarding"], true);
        assert_eq!(data["theme"], "dark");
        assert_eq!(data["effortCalloutDismissed"], true);
        assert_eq!(data["opusProMigrationComplete"], true);
    }

    #[test]
    fn test_ensure_onboarding_noop_when_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join(".claude.json");
        fs::write(
            &state,
            r#"{"hasCompletedOnboarding":true,"theme":"monokai","effortCalloutDismissed":true,"opusProMigrationComplete":true}"#,
        )
        .unwrap();
        ensure_onboarding_complete(&state);
        let data: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(data["theme"], "monokai"); // unchanged
    }

    #[test]
    fn test_ensure_onboarding_merges_missing_theme_preserves_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join(".claude.json");
        fs::write(
            &state,
            r#"{"hasCompletedOnboarding":true,"effortCalloutDismissed":true,"opusProMigrationComplete":true}"#,
        )
        .unwrap();
        ensure_onboarding_complete(&state);
        let data: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(data["theme"], "dark");
        assert_eq!(data["hasCompletedOnboarding"], true);
        assert_eq!(data["effortCalloutDismissed"], true);
        assert_eq!(data["opusProMigrationComplete"], true);
    }

    #[test]
    fn test_ensure_onboarding_preserves_effort_callout_when_theme_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join(".claude.json");
        fs::write(&state, r#"{"hasCompletedOnboarding":true,"effortCalloutDismissed":true}"#)
            .unwrap();
        ensure_onboarding_complete(&state);
        let data: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(data["theme"], "dark");
        assert_eq!(data["effortCalloutDismissed"], true);
        assert_eq!(data["opusProMigrationComplete"], true);
        assert_eq!(data["hasCompletedOnboarding"], true);
    }

    #[test]
    fn test_ensure_onboarding_preserves_custom_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join(".claude.json");
        fs::write(&state, r#"{"theme":"dark","customField":"preserved"}"#).unwrap();
        ensure_onboarding_complete(&state);
        let data: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(data["hasCompletedOnboarding"], true);
        assert_eq!(data["customField"], "preserved");
    }

    #[test]
    fn test_ensure_onboarding_replaces_corrupt_json() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join(".claude.json");
        fs::write(&state, "not valid json{{{").unwrap();
        ensure_onboarding_complete(&state);
        let data: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(data["hasCompletedOnboarding"], true);
        assert_eq!(data["theme"], "dark");
        assert_eq!(data["effortCalloutDismissed"], true);
        assert_eq!(data["opusProMigrationComplete"], true);
    }

    #[test]
    fn test_ensure_onboarding_replaces_dangling_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join(".claude.json");
        let nonexistent = tmp.path().join("nonexistent");
        std::os::unix::fs::symlink(&nonexistent, &state).unwrap();
        assert!(state.symlink_metadata().is_ok()); // symlink exists
        assert!(!state.exists()); // but target doesn't

        ensure_onboarding_complete(&state);
        assert!(state.exists());
        let data: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(data["hasCompletedOnboarding"], true);
        assert_eq!(data["effortCalloutDismissed"], true);
        assert_eq!(data["opusProMigrationComplete"], true);
    }

    #[test]
    fn test_ensure_onboarding_preserves_user_theme() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join(".claude.json");
        fs::write(
            &state,
            r#"{"hasCompletedOnboarding":true,"theme":"monokai","effortCalloutDismissed":true,"opusProMigrationComplete":true,"someOther":42}"#,
        )
        .unwrap();
        ensure_onboarding_complete(&state);
        let data: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(data["theme"], "monokai");
        assert_eq!(data["someOther"], 42);
    }

    #[test]
    fn test_setup_creates_fallback_state_when_no_home_state() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        let result = setup_agent_config_dir("terminal-fallback", repo_root);
        assert!(result.is_some());
        let config_dir = result.unwrap();

        // Even without a home state file, .claude.json should exist
        let state = config_dir.join(".claude.json");
        assert!(state.exists(), ".claude.json should exist as fallback");
        let data: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(data["hasCompletedOnboarding"], true);
        assert_eq!(data["theme"], "dark");
        assert_eq!(data["effortCalloutDismissed"], true);
        assert_eq!(data["opusProMigrationComplete"], true);
    }

    #[test]
    fn test_setup_does_not_overwrite_existing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        // First setup
        let config_dir = setup_agent_config_dir("terminal-6", repo_root).unwrap();

        // Create a custom file at one of the destinations
        let custom_file = config_dir.join("settings.json");
        // Remove any existing file first
        let _ = fs::remove_file(&custom_file);
        fs::write(&custom_file, "custom").unwrap();

        // Second setup should not overwrite the custom file
        setup_agent_config_dir("terminal-6", repo_root);
        let contents = fs::read_to_string(&custom_file).unwrap();
        assert_eq!(contents, "custom");
    }

    #[test]
    fn test_copy_settings_strips_enabled_plugins() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("settings.json");
        fs::write(
            &src,
            r#"{"enabledPlugins":{"rust-analyzer-lsp@official":true},"model":"sonnet","alwaysThinkingEnabled":true}"#,
        )
        .unwrap();

        let dst = tmp.path().join("out.json");
        assert!(copy_settings_without_plugins(&src, &dst));

        let data: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&dst).unwrap()).unwrap();
        assert!(data.get("enabledPlugins").is_none(), "enabledPlugins should be stripped");
        assert_eq!(data["model"], "sonnet");
        assert_eq!(data["alwaysThinkingEnabled"], true);
    }

    #[test]
    fn test_copy_settings_preserves_all_other_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("settings.json");
        fs::write(&src, r#"{"model":"opus","customSetting":42}"#).unwrap();

        let dst = tmp.path().join("out.json");
        assert!(copy_settings_without_plugins(&src, &dst));

        let data: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&dst).unwrap()).unwrap();
        assert_eq!(data["model"], "opus");
        assert_eq!(data["customSetting"], 42);
    }

    #[test]
    fn test_copy_settings_missing_src_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let dst = tmp.path().join("out.json");
        assert!(!copy_settings_without_plugins(&tmp.path().join("nope.json"), &dst));
        assert!(!dst.exists());
    }

    #[test]
    fn test_copy_settings_corrupt_json_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("settings.json");
        fs::write(&src, "not json{{{").unwrap();

        let dst = tmp.path().join("out.json");
        assert!(!copy_settings_without_plugins(&src, &dst));
    }

    #[test]
    fn test_setup_creates_fallback_settings_when_source_missing() {
        // When ~/.claude/settings.json doesn't exist, a minimal fallback
        // should still be written to prevent Claude Code from falling back
        // to the global settings file with enabledPlugins (#3065).
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        let result = setup_agent_config_dir("terminal-fallback-settings", repo_root);
        assert!(result.is_some());
        let config_dir = result.unwrap();

        let settings = config_dir.join("settings.json");
        assert!(
            settings.exists(),
            "Fallback settings.json should be created even when source is missing"
        );

        let data: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert!(
            data.get("enabledPlugins").is_none(),
            "enabledPlugins must not be present in fallback"
        );
    }

    #[test]
    fn test_project_claude_links_constant_includes_commands_and_agents() {
        // Constant must include the two directories Claude Code 2.1+
        // consults when resolving namespaced slash commands.
        // See issue #3346.
        assert!(PROJECT_CLAUDE_LINKS.contains(&"commands"));
        assert!(PROJECT_CLAUDE_LINKS.contains(&"agents"));
    }

    #[test]
    fn test_setup_links_project_commands_when_present() {
        // When `<repo>/.claude/commands` exists, the config dir must
        // contain a symlink to it so `/loom:shepherd` resolves.
        // See issue #3346.
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        let project_commands = repo_root.join(".claude").join("commands").join("loom");
        fs::create_dir_all(&project_commands).unwrap();
        fs::write(project_commands.join("shepherd.md"), "# shepherd\n").unwrap();

        let config_dir = setup_agent_config_dir("shepherd-1", repo_root).unwrap();
        let dst = config_dir.join("commands");
        let meta = fs::symlink_metadata(&dst).unwrap();
        assert!(meta.file_type().is_symlink(), "config_dir/commands must be a symlink");

        // Canonicalize both sides for a stable comparison
        // (tempdir paths on macOS go through /private symlinks).
        let expected = fs::canonicalize(repo_root.join(".claude").join("commands")).unwrap();
        let actual = fs::canonicalize(&dst).unwrap();
        assert_eq!(actual, expected);

        // The role file is reachable through the link.
        assert!(dst.join("loom").join("shepherd.md").is_file());
    }

    #[test]
    fn test_setup_skips_project_link_when_dir_missing() {
        // Repos without `.claude/commands` get no link and no error.
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        let config_dir = setup_agent_config_dir("shepherd-2", repo_root).unwrap();
        assert!(!config_dir.join("commands").exists());
        assert!(!config_dir.join("agents").exists());
    }

    #[test]
    fn test_setup_refreshes_stale_project_link() {
        // If the config dir already has a symlink pointing at the wrong
        // target, setup must replace it with the correct one.
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        let project_commands = repo_root.join(".claude").join("commands");
        fs::create_dir_all(&project_commands).unwrap();

        let config_dir = repo_root
            .join(".loom")
            .join("claude-config")
            .join("shepherd-3");
        fs::create_dir_all(&config_dir).unwrap();

        // Pre-create a wrong-target symlink.
        let wrong = repo_root.join("wrong-target");
        fs::create_dir_all(&wrong).unwrap();
        std::os::unix::fs::symlink(&wrong, config_dir.join("commands")).unwrap();

        let _ = setup_agent_config_dir("shepherd-3", repo_root).unwrap();

        let expected = fs::canonicalize(&project_commands).unwrap();
        let actual = fs::canonicalize(config_dir.join("commands")).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_setup_preserves_non_symlink_directory() {
        // If `commands/` is a real directory (operator placed content
        // there), we must not destroy it.
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        let project_commands = repo_root.join(".claude").join("commands");
        fs::create_dir_all(&project_commands).unwrap();

        let config_dir = repo_root
            .join(".loom")
            .join("claude-config")
            .join("shepherd-4");
        fs::create_dir_all(&config_dir).unwrap();
        let plain = config_dir.join("commands");
        fs::create_dir_all(&plain).unwrap();
        fs::write(plain.join("user-file.md"), "user content\n").unwrap();

        let _ = setup_agent_config_dir("shepherd-4", repo_root).unwrap();

        // Still a plain dir; user content preserved.
        let meta = fs::symlink_metadata(&plain).unwrap();
        assert!(!meta.file_type().is_symlink());
        assert!(plain.join("user-file.md").is_file());
    }

    #[test]
    fn test_link_helper_idempotent() {
        // Calling setup twice with the same project paths is a no-op.
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        let project_commands = repo_root.join(".claude").join("commands");
        fs::create_dir_all(&project_commands).unwrap();

        let first = setup_agent_config_dir("shepherd-5", repo_root).unwrap();
        let first_target = fs::canonicalize(first.join("commands")).unwrap();

        let _ = setup_agent_config_dir("shepherd-5", repo_root).unwrap();
        let second_target = fs::canonicalize(first.join("commands")).unwrap();
        assert_eq!(first_target, second_target);
    }

    #[test]
    fn test_validate_missing_dir_is_unhealthy() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!validate_agent_config_dir("nope", tmp.path()));
    }

    #[test]
    fn test_validate_healthy_after_setup() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        let _ = setup_agent_config_dir("validate-1", repo_root).unwrap();
        assert!(validate_agent_config_dir("validate-1", repo_root));
    }

    #[test]
    fn test_validate_fails_on_missing_settings_json() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        let config_dir = setup_agent_config_dir("validate-2", repo_root).unwrap();
        fs::remove_file(config_dir.join("settings.json")).unwrap();
        assert!(!validate_agent_config_dir("validate-2", repo_root));
    }

    #[test]
    fn test_validate_fails_on_missing_mutable_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        let config_dir = setup_agent_config_dir("validate-3", repo_root).unwrap();
        fs::remove_dir_all(config_dir.join("tmp")).unwrap();
        assert!(!validate_agent_config_dir("validate-3", repo_root));
    }

    #[test]
    fn test_validate_fails_on_dangling_claude_json_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        fs::create_dir_all(repo_root.join(".loom")).unwrap();

        let config_dir = setup_agent_config_dir("validate-4", repo_root).unwrap();
        let state_dst = config_dir.join(".claude.json");
        if state_dst.exists() || fs::symlink_metadata(&state_dst).is_ok() {
            fs::remove_file(&state_dst).unwrap();
        }
        // Point the symlink at a target that doesn't exist.
        std::os::unix::fs::symlink(repo_root.join("does-not-exist"), &state_dst).unwrap();
        assert!(!validate_agent_config_dir("validate-4", repo_root));
    }

    #[test]
    fn test_ensure_project_trusted_is_idempotent_and_merges() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join(".claude.json");
        fs::write(&state_path, r#"{"otherField": "keep-me"}"#).unwrap();

        let project_dir = tmp.path().join("worktree-1");
        ensure_project_trusted(&state_path, &project_dir);

        let contents = fs::read_to_string(&state_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(parsed["otherField"], "keep-me");
        assert_eq!(
            parsed["projects"][project_dir.to_string_lossy().to_string()]["hasTrustDialogAccepted"],
            true
        );

        // Idempotent: calling again does not blow away other fields.
        ensure_project_trusted(&state_path, &project_dir);
        let contents2 = fs::read_to_string(&state_path).unwrap();
        let parsed2: serde_json::Value = serde_json::from_str(&contents2).unwrap();
        assert_eq!(parsed2["otherField"], "keep-me");
    }

    #[test]
    fn test_ensure_project_trusted_writes_through_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real-state.json");
        fs::write(&target, r#"{"otherField": "keep-me"}"#).unwrap();
        let state_path = tmp.path().join(".claude.json");
        std::os::unix::fs::symlink(&target, &state_path).unwrap();

        let project_dir = tmp.path().join("worktree-3");
        ensure_project_trusted(&state_path, &project_dir);

        // Symlink must survive (not replaced by a standalone file).
        assert!(fs::symlink_metadata(&state_path)
            .unwrap()
            .file_type()
            .is_symlink());
        let contents = fs::read_to_string(&target).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(parsed["otherField"], "keep-me");
        assert_eq!(
            parsed["projects"][project_dir.to_string_lossy().to_string()]["hasTrustDialogAccepted"],
            true
        );
    }
}
