use crate::types::{TerminalId, TerminalInfo};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Per-agent `CLAUDE_CONFIG_DIR` isolation.
///
/// Creates isolated Claude Code config directories for each terminal so concurrent
/// terminals don't fight over sessions, lock files, and temp directories in the
/// shared `~/.claude/` directory.
///
/// Mirrors the Python implementation in `loom_tools.common.claude_config`.
mod claude_config {
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
        let hash = format!("{:x}", hasher.finalize());
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

        let cred_hex = hex::encode(cred.as_bytes());

        // Write to the hashed service name
        let write = std::process::Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-a",
                &account,
                "-s",
                &target_service,
                "-X",
                &cred_hex,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        match write {
            Ok(s) if s.success() => {
                log::debug!("Cloned keychain credential to {target_service}");
            }
            _ => {
                log::warn!("Failed to clone keychain credential to {target_service}");
            }
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
            existing_data
                .insert("theme".to_string(), serde_json::Value::String("dark".to_string()));
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
            if settings_src.exists() {
                copy_settings_without_plugins(&settings_src, &settings_dst);
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
                    assert!(
                        dst.symlink_metadata().is_ok(),
                        "Symlink should exist for dir {dirname}"
                    );
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
    }
}

/// Build the pipe-pane command string that strips ANSI escape sequences from output.
///
/// The sed command removes:
/// - Standard ANSI escape sequences: ESC[...letter (colors, cursor, modes)
/// - Terminal mode queries: ESC[?...h/l (like ?2026h/l)
/// - OSC sequences: ESC]...BEL (title setting, etc.)
/// - Carriage returns (\r) from TUI line rewriting
/// - Backspaces (\x08) from cursor corrections
/// - Bare escape sequences (ESC not followed by [ or ]) from raw cursor movement
fn pipe_pane_cmd(output_file: &str) -> String {
    format!("sed -E 's/\\x1b\\[[?0-9;]*[a-zA-Z]//g; s/\\x1b\\][^\\x07]*\\x07//g; s/\\r//g; s/\\x08//g; s/\\x1b[^][]//g' >> {output_file}")
}

pub struct TerminalManager {
    terminals: HashMap<TerminalId, TerminalInfo>,
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            terminals: HashMap::new(),
        }
    }

    /// Validate terminal ID to prevent command injection
    /// Only allows alphanumeric characters, hyphens, and underscores
    fn validate_terminal_id(id: &str) -> Result<()> {
        if id.is_empty() {
            return Err(anyhow!("Terminal ID cannot be empty"));
        }

        if !id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            return Err(anyhow!(
                "Invalid terminal ID: '{id}'. Only alphanumeric characters, hyphens, and underscores are allowed"
            ));
        }

        Ok(())
    }

    /// Derive the repository root from a working directory or `LOOM_WORKSPACE` env var.
    fn find_repo_root(working_dir: Option<&str>) -> Option<PathBuf> {
        working_dir
            .map(Path::new)
            .and_then(|p| {
                p.ancestors()
                    .find(|ancestor| ancestor.join(".loom").is_dir())
            })
            .map(Path::to_path_buf)
            .or_else(|| std::env::var("LOOM_WORKSPACE").ok().map(PathBuf::from))
    }

    /// Set up per-agent `CLAUDE_CONFIG_DIR` isolation for a tmux session.
    ///
    /// Creates an isolated config directory and sets `CLAUDE_CONFIG_DIR` and `TMPDIR`
    /// environment variables on the tmux session.
    fn setup_config_dir_isolation(
        terminal_id: &str,
        working_dir: Option<&str>,
        tmux_session: &str,
    ) {
        let Some(repo_root) = Self::find_repo_root(working_dir) else {
            log::debug!(
                "No repo root found for terminal {terminal_id}; skipping CLAUDE_CONFIG_DIR isolation"
            );
            return;
        };

        let Some(config_dir) = claude_config::setup_agent_config_dir(terminal_id, &repo_root)
        else {
            return;
        };

        let config_dir_str = config_dir.to_string_lossy();
        let tmp_dir_str = config_dir.join("tmp").to_string_lossy().to_string();

        // Set CLAUDE_CONFIG_DIR on the tmux session
        let _ = Command::new("tmux")
            .args(["-L", "loom"])
            .args([
                "set-environment",
                "-t",
                tmux_session,
                "CLAUDE_CONFIG_DIR",
                &config_dir_str,
            ])
            .output();

        // Set TMPDIR on the tmux session
        let _ = Command::new("tmux")
            .args(["-L", "loom"])
            .args([
                "set-environment",
                "-t",
                tmux_session,
                "TMPDIR",
                &tmp_dir_str,
            ])
            .output();

        log::info!("Set CLAUDE_CONFIG_DIR={config_dir_str} for session {tmux_session}");
    }

    /// Handle tmux command errors with consistent logging
    /// Returns true if the error indicates the tmux server is dead
    fn handle_tmux_error(stderr: &str, operation: &str) -> bool {
        if stderr.contains("no server running") {
            log::error!(
                "🚨 TMUX SERVER DEAD during {operation} - Socket should be at /private/tmp/tmux-$UID/loom"
            );
            true
        } else if stderr.contains("no sessions") || stderr.contains("no such session") {
            log::debug!("No tmux sessions found during {operation}: {stderr}");
            false
        } else {
            log::error!("tmux {operation} failed: {stderr}");
            false
        }
    }

    /// Kill the process tree rooted at a tmux session's pane processes.
    ///
    /// When tmux kill-session sends SIGHUP, it doesn't propagate across process group
    /// boundaries. The `claude` CLI is typically behind a wrapper/timeout chain that
    /// creates separate process groups, so it survives session destruction as an orphan.
    ///
    /// This method kills the entire process tree first (SIGTERM then SIGKILL escalation),
    /// then destroys the tmux session.
    fn kill_process_tree(session_name: &str, force: bool) {
        // Get pane PIDs for this session
        let pane_output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["list-panes", "-t", session_name, "-F", "#{pane_pid}"])
            .output();

        let pane_pids: Vec<String> = match pane_output {
            Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(std::string::ToString::to_string)
                .collect(),
            _ => {
                log::debug!("Could not get pane PIDs for session {session_name}");
                Vec::new()
            }
        };

        if pane_pids.is_empty() {
            return;
        }

        // Collect all descendant PIDs (depth-first for bottom-up kill)
        let mut all_pids: Vec<String> = Vec::new();
        for pane_pid in &pane_pids {
            Self::collect_descendants(pane_pid, &mut all_pids);
            all_pids.push(pane_pid.clone());
        }

        if all_pids.is_empty() {
            return;
        }

        log::info!(
            "Killing process tree for session {session_name}: {} process(es)",
            all_pids.len()
        );

        if force {
            // Force mode: SIGKILL immediately
            for pid in &all_pids {
                let _ = Command::new("kill").args(["-9", pid]).output();
            }
        } else {
            // Graceful mode: SIGTERM first
            for pid in &all_pids {
                let _ = Command::new("kill").args(["-15", pid]).output();
            }

            // Brief wait for processes to terminate
            std::thread::sleep(std::time::Duration::from_secs(1));

            // Escalate to SIGKILL for any survivors
            for pid in &all_pids {
                // Check if process is still alive (kill -0)
                if Command::new("kill")
                    .args(["-0", pid])
                    .output()
                    .is_ok_and(|o| o.status.success())
                {
                    let _ = Command::new("kill").args(["-9", pid]).output();
                }
            }
        }
    }

    /// Recursively collect all descendant PIDs of a given PID (depth-first)
    fn collect_descendants(parent_pid: &str, pids: &mut Vec<String>) {
        let output = Command::new("pgrep").args(["-P", parent_pid]).output();

        if let Ok(output) = output {
            if output.status.success() {
                let children: Vec<String> = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(std::string::ToString::to_string)
                    .collect();

                for child in &children {
                    // Recurse into grandchildren first (depth-first)
                    Self::collect_descendants(child, pids);
                    pids.push(child.clone());
                }
            }
        }
    }

    pub fn create_terminal(
        &mut self,
        config_id: &str,
        name: String,
        working_dir: Option<String>,
        role: Option<&String>,
        instance_number: Option<u32>,
    ) -> Result<TerminalId> {
        // Validate terminal ID to prevent command injection
        Self::validate_terminal_id(config_id)?;

        // Use config_id directly as the terminal ID
        let id = config_id.to_string();
        let role_part = role.map_or("default", String::as_str);
        let instance_part = instance_number.unwrap_or(0);
        let tmux_session = format!("loom-{id}-{role_part}-{instance_part}");

        log::info!("Creating tmux session: {tmux_session}, working_dir: {working_dir:?}");

        // First, verify tmux server is responsive
        let check_output = Command::new("tmux")
            .args(["-L", "loom", "list-sessions"])
            .output();

        match check_output {
            Ok(out) if !out.status.success() => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                // Special case: constructor only warns about server not running
                if stderr.contains("no server running") {
                    log::warn!("tmux server not running, will start on first session creation");
                } else {
                    Self::handle_tmux_error(&stderr, "new");
                }
            }
            Err(e) => {
                log::error!("Failed to check tmux server status: {e}");
            }
            _ => {}
        }

        let mut cmd = Command::new("tmux");
        cmd.args(["-L", "loom"]);
        cmd.args([
            "new-session",
            "-d",
            "-s",
            &tmux_session,
            "-x",
            "80", // Standard width: 80 columns
            "-y",
            "24", // Standard height: 24 rows
        ]);

        if let Some(dir) = &working_dir {
            cmd.args(["-c", dir]);
        }

        log::info!("About to spawn tmux command...");
        let result = cmd.spawn()?.wait()?;
        log::info!("Tmux command completed with status: {result}");

        if !result.success() {
            // Get more details about the failure
            let stderr_output = Command::new("tmux")
                .args(["-L", "loom", "list-sessions"])
                .output();

            if let Ok(out) = stderr_output {
                let stderr = String::from_utf8_lossy(&out.stderr);
                log::error!("tmux session creation failed. Server status: {stderr}");
            }

            return Err(anyhow!("Failed to create tmux session '{tmux_session}'"));
        }

        // Set up pipe-pane to capture output with ANSI stripping
        let output_file = format!("/tmp/loom-{id}.out");
        let pipe_cmd = pipe_pane_cmd(&output_file);

        log::info!("Setting up pipe-pane for session {tmux_session} to {output_file}");
        let result = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["pipe-pane", "-t", &tmux_session, "-o", &pipe_cmd])
            .output()?;

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            log::error!("pipe-pane failed for session {tmux_session}: {stderr}");

            // Check if session still exists
            let check = Command::new("tmux")
                .args(["-L", "loom", "has-session", "-t", &tmux_session])
                .output();

            if let Ok(out) = check {
                if out.status.success() {
                    log::error!("Session {tmux_session} exists but pipe-pane setup failed");
                } else {
                    log::error!("Session {tmux_session} disappeared during pipe-pane setup!");
                }
            }

            return Err(anyhow!("Failed to set up pipe-pane for {tmux_session}: {stderr}"));
        }
        log::info!("pipe-pane setup successful for session {tmux_session}");

        // Set up per-agent CLAUDE_CONFIG_DIR isolation
        Self::setup_config_dir_isolation(&id, working_dir.as_deref(), &tmux_session);

        let info = TerminalInfo {
            id: id.clone(),
            name,
            tmux_session,
            working_dir,
            created_at: chrono::Utc::now().timestamp(),
            role: role.cloned(),
            worktree_path: None,
            agent_pid: None,
            agent_status: crate::types::AgentStatus::default(),
            last_interval_run: None,
        };

        self.terminals.insert(id.clone(), info);
        Ok(id)
    }

    pub fn list_terminals(&mut self) -> Vec<TerminalInfo> {
        // If registry is empty but tmux sessions exist, restore from tmux
        // Skip restore when LOOM_NO_RESTORE=1 is set (used in tests to prevent
        // cross-test-binary contamination via shared tmux server)
        let no_restore = std::env::var("LOOM_NO_RESTORE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        if self.terminals.is_empty() && !no_restore {
            log::debug!("Registry empty, attempting to restore from tmux");
            if let Err(e) = self.restore_from_tmux() {
                log::warn!("Failed to restore terminals from tmux: {e}");
            }
        }
        self.terminals.values().cloned().collect()
    }

    pub fn set_worktree_path(&mut self, id: &TerminalId, worktree_path: &str) -> Result<()> {
        let info = self
            .terminals
            .get_mut(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        info.worktree_path = Some(worktree_path.to_string());

        // Set LOOM_WORKTREE_PATH on the tmux session so Claude Code's
        // PreToolUse hook can block Edit/Write outside the worktree (issue #2441).
        let _ = Command::new("tmux")
            .args(["-L", "loom"])
            .args([
                "set-environment",
                "-t",
                &info.tmux_session,
                "LOOM_WORKTREE_PATH",
                worktree_path,
            ])
            .output();

        log::info!("Set worktree path for terminal {id}: {worktree_path}");
        Ok(())
    }

    pub fn destroy_terminal(&mut self, id: &TerminalId) -> Result<()> {
        let info = self
            .terminals
            .get(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        // Capture worktree info before killing the session.
        // We need to kill the tmux session FIRST to avoid leaving the shell's
        // CWD pointing at a deleted worktree path (see issue #2413).
        let worktree_to_remove: Option<(String, PathBuf)> =
            if let Some(ref worktree_path) = info.worktree_path {
                let path = PathBuf::from(worktree_path);
                if path.to_string_lossy().contains(".loom/worktrees") {
                    let other_users = self
                        .terminals
                        .values()
                        .filter(|t| t.id != *id && t.worktree_path.as_ref() == Some(worktree_path))
                        .count();

                    if other_users == 0 {
                        Some((worktree_path.clone(), path))
                    } else {
                        log::info!(
                            "Skipping worktree removal at {} ({} other terminal(s) still using it)",
                            path.display(),
                            other_users
                        );
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };

        // Stop pipe-pane (passing no command closes the pipe)
        let _ = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["pipe-pane", "-t", &info.tmux_session])
            .spawn();

        // Kill process tree before destroying tmux session
        // This prevents orphaned claude processes that survive SIGHUP
        Self::kill_process_tree(&info.tmux_session, false);

        // Kill the tmux session (may already be dead from kill_process_tree)
        let _ = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["kill-session", "-t", &info.tmux_session])
            .spawn()
            .and_then(|mut c| c.wait());

        // Now that the tmux session is dead, safe to remove the worktree
        // without breaking any shell's working directory.
        if let Some((worktree_path, path)) = worktree_to_remove {
            log::info!("Removing worktree at {} (no other terminals using it)", path.display());

            // Derive repo root from worktree path (e.g., /repo/.loom/worktrees/issue-42 → /repo)
            // Run git from repo root to avoid CWD-inside-worktree issues
            let repo_root = path
                .ancestors()
                .find(|p| p.join(".loom").is_dir() && p.join(".git").exists())
                .map(std::path::Path::to_path_buf);

            // First try to remove the worktree via git
            let mut cmd = Command::new("git");
            cmd.args(["worktree", "remove", &worktree_path]);
            if let Some(ref root) = repo_root {
                cmd.current_dir(root);
            }
            let output = cmd.output();

            if let Ok(output) = output {
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    log::warn!("git worktree remove failed: {stderr}");
                    log::info!("Attempting force removal...");

                    // Try force removal
                    let mut cmd = Command::new("git");
                    cmd.args(["worktree", "remove", "--force", &worktree_path]);
                    if let Some(ref root) = repo_root {
                        cmd.current_dir(root);
                    }
                    let _ = cmd.output();
                }
            }

            // Also try to remove directory manually as fallback
            let _ = fs::remove_dir_all(&path);
        }

        // Clean up the output file
        let output_file = format!("/tmp/loom-{id}.out");
        let _ = std::fs::remove_file(output_file);

        // Clean up per-agent CLAUDE_CONFIG_DIR
        if let Some(root) = Self::find_repo_root(info.working_dir.as_deref()) {
            if claude_config::cleanup_agent_config_dir(id, &root) {
                log::info!("Cleaned up CLAUDE_CONFIG_DIR for terminal {id}");
            }
        }

        self.terminals.remove(id);
        Ok(())
    }

    pub fn send_input(&self, id: &TerminalId, data: &str) -> Result<()> {
        let info = self
            .terminals
            .get(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        match data {
            "\r" => {
                Command::new("tmux")
                    .args(["-L", "loom"])
                    .args(["send-keys", "-t", &info.tmux_session, "Enter"])
                    .spawn()?;
            }
            "\u{0003}" => {
                Command::new("tmux")
                    .args(["-L", "loom"])
                    .args(["send-keys", "-t", &info.tmux_session, "C-c"])
                    .spawn()?;
            }
            _ => {
                Command::new("tmux")
                    .args(["-L", "loom"])
                    .args(["send-keys", "-t", &info.tmux_session, "-l", data])
                    .spawn()?;
            }
        }

        Ok(())
    }

    #[allow(clippy::cast_possible_truncation, clippy::unused_self)]
    pub fn get_terminal_output(
        &self,
        id: &TerminalId,
        start_byte: Option<usize>,
    ) -> Result<(Vec<u8>, usize)> {
        use std::fs;
        use std::io::{Read, Seek};

        // Use config_id directly for filename
        let output_file = format!("/tmp/loom-{id}.out");
        log::debug!("Reading terminal output from: {output_file}");

        let mut file = match fs::File::open(&output_file) {
            Ok(f) => f,
            Err(e) => {
                // File doesn't exist yet, return empty
                log::debug!("Output file doesn't exist yet: {e}");
                return Ok((Vec::new(), 0));
            }
        };

        // Get file size
        let metadata = file.metadata()?;
        let file_size = metadata.len() as usize;
        log::debug!("Output file size: {file_size} bytes");

        // If start_byte is specified, seek to that position and read from there
        let bytes_to_read = if let Some(start) = start_byte {
            if start >= file_size {
                // No new data
                log::debug!("No new data (start_byte={start} >= file_size={file_size})");
                return Ok((Vec::new(), file_size));
            }
            file.seek(std::io::SeekFrom::Start(start as u64))?;
            let bytes = file_size - start;
            log::debug!("Seeking to byte {start} and reading {bytes} bytes");
            file_size - start
        } else {
            // Read entire file
            log::debug!("Reading entire file ({file_size} bytes)");
            file_size
        };

        let mut buffer = vec![0u8; bytes_to_read];
        file.read_exact(&mut buffer)?;
        log::debug!("Read {len} bytes successfully", len = buffer.len());

        Ok((buffer, file_size))
    }

    pub fn resize_terminal(&self, id: &TerminalId, cols: u16, rows: u16) -> Result<()> {
        let info = self
            .terminals
            .get(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        // Resize tmux window (which resizes the pane when there's only one pane)
        Command::new("tmux")
            .args(["-L", "loom"])
            .args([
                "resize-window",
                "-t",
                &info.tmux_session,
                "-x",
                &cols.to_string(),
                "-y",
                &rows.to_string(),
            ])
            .spawn()?
            .wait()?;

        Ok(())
    }

    /// Restore terminals from existing tmux sessions.
    ///
    /// By default (no filter), imports ALL `loom-*` sessions for backward compatibility.
    /// When a filter is provided via `restore_from_tmux_with_filter`, only sessions
    /// matching configured terminal IDs are restored. This prevents importing stale
    /// sessions from crashed daemons or other daemon instances.
    pub fn restore_from_tmux(&mut self) -> Result<()> {
        self.restore_from_tmux_with_filter(None)
    }

    /// Restore terminals from existing tmux sessions, optionally filtering by configured IDs.
    ///
    /// # Arguments
    /// * `configured_ids` - If Some, only restore sessions whose extracted terminal ID
    ///   matches one of the configured IDs. If None, restore all loom-* sessions.
    ///
    /// # Session Ownership (Issue #1952)
    /// Without filtering, the daemon imports ANY `loom-*` session, which causes:
    /// - Test interference between different test binaries
    /// - Stale session accumulation from crashed daemons
    /// - No ownership verification between daemon instances
    ///
    /// With filtering (recommended), only sessions matching the workspace's config.json
    /// terminal definitions are restored, providing configuration-based ownership.
    pub fn restore_from_tmux_with_filter(
        &mut self,
        configured_ids: Option<&std::collections::HashSet<String>>,
    ) -> Result<()> {
        let output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()?;

        // Enhanced logging: Check for tmux server failure
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Self::handle_tmux_error(&stderr, "restore_from_tmux");

            // Return early with empty list if server is dead
            return Ok(());
        }

        let sessions = String::from_utf8_lossy(&output.stdout);
        let session_count = sessions.lines().count();
        log::info!("📊 tmux server status: {session_count} total sessions");

        if let Some(ids) = configured_ids {
            log::info!(
                "🔒 Configuration-based restore: filtering to {} configured terminal(s)",
                ids.len()
            );
        } else {
            log::debug!("📦 Legacy restore: importing all loom-* sessions (no filter)");
        }

        let mut restored_count = 0;
        let mut skipped_count = 0;

        for session in sessions.lines() {
            if let Some(remainder) = session.strip_prefix("loom-") {
                // Session format: loom-{config_id}-{role}-{instance}
                // Extract config_id by checking for the "terminal-{number}" pattern
                //
                // Example session: loom-terminal-1-claude-code-worker-64
                // After strip_prefix: terminal-1-claude-code-worker-64
                // Split by '-': ["terminal", "1", "claude", "code", "worker", "64"]
                //
                // Strategy: If format is "terminal-{number}-...", extract "terminal-{number}"
                // Otherwise use first part for backwards compatibility

                let parts: Vec<&str> = remainder.split('-').collect();
                if parts.is_empty() {
                    log::warn!("Skipping malformed session name: {session}");
                    continue;
                }

                // Check if this matches the "terminal-{number}" pattern
                let id = if parts.len() >= 2
                    && parts[0] == "terminal"
                    && parts[1].chars().all(|c| c.is_ascii_digit())
                {
                    // Format: terminal-{number}-{role}-{instance}
                    // Extract "terminal-{number}" as the ID
                    format!("{}-{}", parts[0], parts[1])
                } else {
                    // For backwards compatibility with old format (no hyphens in ID),
                    // use first part as the terminal ID
                    parts[0].to_string()
                };

                // Validate terminal ID to prevent command injection
                if let Err(e) = Self::validate_terminal_id(&id) {
                    log::warn!("Skipping invalid terminal ID from tmux session {session}: {e}");
                    continue;
                }

                // If filter is provided, skip sessions not in the configured set
                if let Some(ids) = configured_ids {
                    if !ids.contains(&id) {
                        log::debug!(
                            "Skipping unconfigured session {session} (terminal ID '{id}' not in config)"
                        );
                        skipped_count += 1;
                        continue;
                    }
                }

                // Clear any existing pipe-pane for this session to avoid duplicates
                log::debug!("Clearing existing pipe-pane for session {session}");
                let _ = Command::new("tmux")
                    .args(["-L", "loom"])
                    .args(["pipe-pane", "-t", session])
                    .spawn();

                // Set up fresh pipe-pane to capture output with ANSI stripping
                let output_file = format!("/tmp/loom-{id}.out");
                let pipe_cmd = pipe_pane_cmd(&output_file);

                log::info!("Setting up pipe-pane for session {session} to {output_file}");
                let result = Command::new("tmux")
                    .args(["-L", "loom"])
                    .args(["pipe-pane", "-t", session, "-o", &pipe_cmd])
                    .output()?;

                if result.status.success() {
                    log::info!("pipe-pane setup successful for {session}");
                } else {
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    log::warn!("pipe-pane setup failed for {session}: {stderr}");
                    // Continue anyway - terminal is still usable
                }

                self.terminals
                    .entry(id.clone())
                    .or_insert_with(|| TerminalInfo {
                        id: id.clone(),
                        name: format!("Restored: {session}"),
                        tmux_session: session.to_string(),
                        working_dir: None,
                        created_at: chrono::Utc::now().timestamp(),
                        role: None,
                        worktree_path: None,
                        agent_pid: None,
                        agent_status: crate::types::AgentStatus::default(),
                        last_interval_run: None,
                    });

                restored_count += 1;
            }
        }

        if configured_ids.is_some() {
            log::info!(
                "📊 Restore complete: {restored_count} restored, {skipped_count} skipped (unconfigured)"
            );
        }

        Ok(())
    }

    /// Clean up stale tmux sessions from previous daemon runs.
    /// Lists all loom-* sessions and kills any that weren't restored
    /// into the terminal registry by `restore_from_tmux()`.
    pub fn clean_stale_sessions(&self) -> Result<usize> {
        let output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("no server running") || stderr.contains("no sessions") {
                return Ok(0);
            }
            log::warn!("Failed to list tmux sessions for cleanup: {stderr}");
            return Ok(0);
        }

        let sessions = String::from_utf8_lossy(&output.stdout);

        // Build a set of tmux session names that are tracked in the registry
        let tracked_sessions: std::collections::HashSet<&str> = self
            .terminals
            .values()
            .map(|t| t.tmux_session.as_str())
            .collect();

        let mut cleaned = 0;
        for session in sessions.lines() {
            if !session.starts_with("loom-") {
                continue;
            }

            if tracked_sessions.contains(session) {
                continue;
            }

            log::info!("Cleaning stale tmux session: {session}");

            // Kill process tree before destroying the stale session
            Self::kill_process_tree(session, true);

            // Force kill the session (may already be dead)
            let result = Command::new("tmux")
                .args(["-L", "loom"])
                .args(["kill-session", "-t", session])
                .output();

            match result {
                Ok(out) if out.status.success() => {
                    cleaned += 1;
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    // Session may already be dead from kill_process_tree
                    if !stderr.contains("no such session") {
                        log::warn!("Failed to kill stale session {session}: {stderr}");
                    }
                    cleaned += 1;
                }
                Err(e) => {
                    log::warn!("Failed to kill stale session {session}: {e}");
                }
            }
        }

        Ok(cleaned)
    }

    /// Check if a tmux session exists for the given terminal ID
    pub fn has_tmux_session(&self, id: &TerminalId) -> Result<bool> {
        log::info!("🔍 has_tmux_session called for terminal id: '{id}'");
        log::info!(
            "📋 Registry has {} terminals: {:?}",
            self.terminals.len(),
            self.terminals.keys().collect::<Vec<_>>()
        );

        // First check if we have this terminal registered
        if let Some(info) = self.terminals.get(id) {
            // Terminal is registered - check its specific tmux session
            log::info!(
                "✅ Terminal '{}' found in registry, checking session: '{}'",
                id,
                info.tmux_session
            );
            let output = Command::new("tmux")
                .args(["-L", "loom"])
                .args(["has-session", "-t", &info.tmux_session])
                .output()?;

            let result = output.status.success();
            log::info!("📊 tmux has-session result for '{}': {}", info.tmux_session, result);
            return Ok(result);
        }

        // Terminal not registered yet - check if ANY loom session with this ID exists
        // This handles the race condition where frontend creates state before daemon registers
        log::warn!("⚠️  Terminal '{id}' NOT found in registry, checking tmux sessions directly");

        let output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Self::handle_tmux_error(&stderr, "has_tmux_session");

            return Ok(false);
        }

        let sessions = String::from_utf8_lossy(&output.stdout);
        let prefix = format!("loom-{id}-");

        // Check if any session matches our terminal ID prefix
        let has_session = sessions.lines().any(|s| s.starts_with(&prefix));

        log::debug!(
            "Terminal {id} tmux session check (unregistered): {}",
            if has_session { "found" } else { "not found" }
        );

        Ok(has_session)
    }

    /// List all available loom tmux sessions
    #[allow(clippy::unused_self)]
    pub fn list_available_sessions(&self) -> Vec<String> {
        let output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["list-sessions", "-F", "#{session_name}"])
            .output();

        // If tmux list-sessions fails (no server running), return empty vec
        let Ok(output) = output else {
            log::error!("Failed to execute tmux list-sessions command");
            return Vec::new();
        };

        // Enhanced logging: Check for tmux server failure
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Self::handle_tmux_error(&stderr, "list_available_sessions");

            return Vec::new();
        }

        let sessions = String::from_utf8_lossy(&output.stdout);
        let loom_sessions: Vec<String> = sessions
            .lines()
            .filter(|s| s.starts_with("loom-"))
            .map(std::string::ToString::to_string)
            .collect();

        log::info!("📊 Found {} loom sessions", loom_sessions.len());
        loom_sessions
    }

    /// Attach an existing terminal record to a different tmux session
    pub fn attach_to_session(&mut self, id: &TerminalId, session_name: String) -> Result<()> {
        let info = self
            .terminals
            .get_mut(id)
            .ok_or_else(|| anyhow!("Terminal not found"))?;

        // Verify the session exists
        let output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["has-session", "-t", &session_name])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            // Enhanced logging: Distinguish failure modes
            if Self::handle_tmux_error(&stderr, "attach_to_session") {
                return Err(anyhow!("tmux server is not running"));
            }

            log::error!("tmux has-session failed for '{session_name}': {stderr}");
            return Err(anyhow!("Tmux session '{session_name}' does not exist"));
        }

        // Update the terminal info to point to the new session
        info.tmux_session = session_name;

        Ok(())
    }

    /// Kill a tmux session by name
    #[allow(clippy::unused_self)]
    pub fn kill_session(&self, session_name: &str) -> Result<()> {
        // Verify the session exists
        let check_output = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["has-session", "-t", session_name])
            .output()?;

        if !check_output.status.success() {
            let stderr = String::from_utf8_lossy(&check_output.stderr);

            // Enhanced logging: Distinguish failure modes
            if Self::handle_tmux_error(&stderr, "kill_session") {
                return Err(anyhow!("tmux server is not running"));
            }

            log::error!("tmux has-session failed for '{session_name}': {stderr}");
            return Err(anyhow!("Tmux session '{session_name}' does not exist"));
        }

        // Kill process tree before destroying the session
        Self::kill_process_tree(session_name, false);

        // Kill the session (may already be dead from kill_process_tree)
        let _ = Command::new("tmux")
            .args(["-L", "loom"])
            .args(["kill-session", "-t", session_name])
            .spawn()
            .and_then(|mut c| c.wait());

        log::info!("Killed tmux session: {session_name}");
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ===== pipe_pane_cmd tests =====

    #[test]
    fn test_pipe_pane_cmd_contains_output_file() {
        let cmd = pipe_pane_cmd("/tmp/loom-test.out");
        assert!(cmd.contains("/tmp/loom-test.out"));
    }

    #[test]
    fn test_pipe_pane_cmd_uses_sed() {
        let cmd = pipe_pane_cmd("/tmp/output.out");
        assert!(cmd.starts_with("sed "));
    }

    #[test]
    fn test_pipe_pane_cmd_strips_ansi_escapes() {
        let cmd = pipe_pane_cmd("/tmp/output.out");
        // Should contain the ANSI escape stripping pattern
        assert!(cmd.contains("\\x1b"));
    }

    #[test]
    fn test_pipe_pane_cmd_appends_to_file() {
        let cmd = pipe_pane_cmd("/tmp/output.out");
        assert!(cmd.contains(">> /tmp/output.out"));
    }

    // ===== validate_terminal_id tests =====

    #[test]
    fn test_validate_terminal_id_empty() {
        let result = TerminalManager::validate_terminal_id("");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));
    }

    #[test]
    fn test_validate_terminal_id_valid_alphanumeric() {
        assert!(TerminalManager::validate_terminal_id("terminal1").is_ok());
    }

    #[test]
    fn test_validate_terminal_id_valid_with_hyphens() {
        assert!(TerminalManager::validate_terminal_id("terminal-1").is_ok());
    }

    #[test]
    fn test_validate_terminal_id_valid_with_underscores() {
        assert!(TerminalManager::validate_terminal_id("terminal_1").is_ok());
    }

    #[test]
    fn test_validate_terminal_id_rejects_spaces() {
        let result = TerminalManager::validate_terminal_id("terminal 1");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid terminal ID"));
    }

    #[test]
    fn test_validate_terminal_id_rejects_special_chars() {
        for id in &[
            "term;rm -rf /",
            "term$(cmd)",
            "term`cmd`",
            "term|pipe",
            "a/b",
        ] {
            assert!(
                TerminalManager::validate_terminal_id(id).is_err(),
                "Expected rejection for: {id}"
            );
        }
    }

    #[test]
    fn test_validate_terminal_id_rejects_dots() {
        assert!(TerminalManager::validate_terminal_id("terminal.1").is_err());
    }

    // ===== handle_tmux_error tests =====

    #[test]
    fn test_handle_tmux_error_no_server_returns_true() {
        let result = TerminalManager::handle_tmux_error("no server running on /tmp/tmux", "test");
        assert!(result, "Should return true when server is dead");
    }

    #[test]
    fn test_handle_tmux_error_no_sessions_returns_false() {
        let result = TerminalManager::handle_tmux_error("no sessions", "test");
        assert!(!result);
    }

    #[test]
    fn test_handle_tmux_error_no_such_session_returns_false() {
        let result = TerminalManager::handle_tmux_error("no such session: loom-test", "test");
        assert!(!result);
    }

    #[test]
    fn test_handle_tmux_error_other_error_returns_false() {
        let result = TerminalManager::handle_tmux_error("some other tmux error", "test");
        assert!(!result);
    }

    // ===== TerminalManager::new tests =====

    #[test]
    fn test_terminal_manager_new_is_empty() {
        let tm = TerminalManager::new();
        assert!(tm.terminals.is_empty());
    }
}
