//! Repository scaffolding setup
//!
//! Sets up CLAUDE.md, .claude/, .codex/, and .github/ directories.

use std::fs;
use std::path::Path;

use serde_json::Value;

use super::file_ops::{copy_dir_with_report, force_merge_dir_with_report, merge_dir_with_report};
use super::git::extract_repo_info;
use super::templates::{substitute_template_variables, LoomMetadata};
use super::InitReport;

/// Prefix used to identify Loom-owned hooks in settings.json.
/// Hooks with commands starting with this prefix are managed by Loom.
pub const LOOM_HOOK_PREFIX: &str = ".loom/hooks/";

/// Loom section markers for CLAUDE.md content preservation
pub const LOOM_SECTION_START: &str = "<!-- BEGIN LOOM ORCHESTRATION -->";
pub const LOOM_SECTION_END: &str = "<!-- END LOOM ORCHESTRATION -->";

/// The short pointer injected into root CLAUDE.md (between section markers).
///
/// The full Loom guide is written to `.loom/CLAUDE.md` in the target repo.
/// Claude Code auto-discovers `.loom/CLAUDE.md` when agents work in
/// `.loom/worktrees/issue-N/` via ancestor directory traversal.
pub const LOOM_ROOT_POINTER: &str = "This repository uses [Loom](https://github.com/rjwalters/loom) for AI-powered development orchestration. See `.loom/CLAUDE.md` for the full guide (roles, labels, worktrees, configuration).";

/// Wrap Loom content in section markers
pub fn wrap_loom_content(content: &str) -> String {
    format!("{}\n{}\n{}", LOOM_SECTION_START, content.trim(), LOOM_SECTION_END)
}

/// Read and parse an existing settings.json file, returning None if missing or invalid.
fn read_existing_settings(path: &Path) -> Option<Value> {
    let content = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;
    if value.is_object() {
        Some(value)
    } else {
        None
    }
}

/// Deep-merge Loom's default settings.json into an existing project settings.json.
///
/// Merge strategy:
/// - **Hooks**: For each hook type (e.g., `PreToolUse`), for each matcher entry,
///   merge Loom hooks alongside existing hooks. Deduplicates by command path.
///   Preserves all project hook types and matchers that Loom doesn't define.
/// - **Permissions**: Union of `permissions.allow` arrays (dedup exact strings).
/// - **Other keys**: Preserves all keys from the existing settings that Loom doesn't define.
pub fn merge_settings_json(existing: &Value, loom_defaults: &Value) -> Value {
    let mut result = existing.clone();
    let Some(result_obj) = result.as_object_mut() else {
        return loom_defaults.clone();
    };

    // Merge hooks
    if let Some(loom_hooks) = loom_defaults.get("hooks").and_then(|h| h.as_object()) {
        let merged_hooks =
            merge_hooks(existing.get("hooks").and_then(|h| h.as_object()), loom_hooks);
        result_obj.insert("hooks".to_string(), Value::Object(merged_hooks));
    }

    // Merge permissions
    if let Some(loom_perms) = loom_defaults.get("permissions").and_then(|p| p.as_object()) {
        let merged_perms =
            merge_permissions(existing.get("permissions").and_then(|p| p.as_object()), loom_perms);
        result_obj.insert("permissions".to_string(), Value::Object(merged_perms));
    }

    result
}

/// Merge hooks from Loom defaults into existing hooks.
///
/// For each hook type in Loom defaults:
///   - For each matcher entry, find matching entry in existing (same matcher value)
///     - If found: merge hooks arrays, deduplicating by command path
///     - If not found: add the entire matcher entry
///   - All existing hook types not in Loom defaults are preserved unchanged
fn merge_hooks(
    existing: Option<&serde_json::Map<String, Value>>,
    loom: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    let mut result = existing.cloned().unwrap_or_default();

    for (hook_type, loom_matchers) in loom {
        let Some(loom_matchers_arr) = loom_matchers.as_array() else {
            continue;
        };

        let existing_matchers = result
            .entry(hook_type.clone())
            .or_insert_with(|| Value::Array(Vec::new()));

        let Some(existing_arr) = existing_matchers.as_array_mut() else {
            continue;
        };

        for loom_matcher_entry in loom_matchers_arr {
            let loom_matcher_val = loom_matcher_entry
                .get("matcher")
                .and_then(|m| m.as_str())
                .unwrap_or("");

            // Find matching entry in existing
            let found = existing_arr.iter_mut().find(|entry| {
                entry.get("matcher").and_then(|m| m.as_str()).unwrap_or("") == loom_matcher_val
            });

            if let Some(existing_entry) = found {
                // Merge hooks arrays within this matcher entry
                merge_hook_commands(existing_entry, loom_matcher_entry);
            } else {
                // No matching entry exists - add the entire matcher entry
                existing_arr.push(loom_matcher_entry.clone());
            }
        }
    }

    result
}

/// Merge hook commands within a single matcher entry, deduplicating by command path.
fn merge_hook_commands(existing_entry: &mut Value, loom_entry: &Value) {
    let Some(existing_hooks) = existing_entry
        .get_mut("hooks")
        .and_then(|h| h.as_array_mut())
    else {
        return;
    };

    let Some(loom_hooks) = loom_entry.get("hooks").and_then(|h| h.as_array()) else {
        return;
    };

    // Collect existing command paths for dedup
    let existing_commands: std::collections::HashSet<String> = existing_hooks
        .iter()
        .filter_map(|h| h.get("command").and_then(|c| c.as_str()).map(String::from))
        .collect();

    // Add Loom hooks that aren't already present
    for loom_hook in loom_hooks {
        let cmd = loom_hook
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        if !existing_commands.contains(cmd) {
            existing_hooks.push(loom_hook.clone());
        }
    }
}

/// Merge permissions, unioning the allow arrays.
fn merge_permissions(
    existing: Option<&serde_json::Map<String, Value>>,
    loom: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    let mut result = existing.cloned().unwrap_or_default();

    if let Some(loom_allow) = loom.get("allow").and_then(|a| a.as_array()) {
        let existing_allow = result
            .entry("allow".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));

        if let Some(existing_arr) = existing_allow.as_array_mut() {
            let existing_set: std::collections::HashSet<String> = existing_arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();

            for perm in loom_allow {
                if let Some(perm_str) = perm.as_str() {
                    if !existing_set.contains(perm_str) {
                        existing_arr.push(perm.clone());
                    }
                }
            }
        }
    }

    result
}

/// Remove Loom-owned hooks from a settings.json value.
///
/// Loom hooks are identified by command paths starting with `.loom/hooks/`.
/// After removal, empty matcher entries and empty hook type arrays are cleaned up.
pub fn remove_loom_hooks(settings: &mut Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return;
    };

    // Process each hook type
    let hook_types: Vec<String> = hooks.keys().cloned().collect();
    for hook_type in &hook_types {
        let Some(matchers) = hooks.get_mut(hook_type).and_then(|m| m.as_array_mut()) else {
            continue;
        };

        // For each matcher entry, remove Loom hooks from the hooks array
        for matcher_entry in matchers.iter_mut() {
            if let Some(hook_arr) = matcher_entry
                .get_mut("hooks")
                .and_then(|h| h.as_array_mut())
            {
                hook_arr.retain(|hook| {
                    let cmd = hook.get("command").and_then(|c| c.as_str()).unwrap_or("");
                    !cmd.starts_with(LOOM_HOOK_PREFIX)
                });
            }
        }

        // Remove matcher entries with empty hooks arrays
        matchers.retain(|entry| {
            !entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .is_some_and(Vec::is_empty)
        });
    }

    // Remove hook types with empty matcher arrays
    hooks.retain(|_, v| !v.as_array().is_some_and(Vec::is_empty));

    // If hooks object is now empty, remove it entirely
    if hooks.is_empty() {
        if let Some(obj) = settings.as_object_mut() {
            obj.remove("hooks");
        }
    }
}

/// Remove Loom-specific permissions from a settings.json value.
///
/// Removes permissions that match Loom's default permission list exactly.
pub fn remove_loom_permissions(settings: &mut Value, loom_defaults: &Value) {
    let Some(loom_perms) = loom_defaults
        .get("permissions")
        .and_then(|p| p.get("allow"))
        .and_then(|a| a.as_array())
    else {
        return;
    };

    let loom_perm_set: std::collections::HashSet<&str> =
        loom_perms.iter().filter_map(|v| v.as_str()).collect();

    let Some(allow) = settings
        .get_mut("permissions")
        .and_then(|p| p.get_mut("allow"))
        .and_then(|a| a.as_array_mut())
    else {
        return;
    };

    allow.retain(|v| !v.as_str().is_some_and(|s| loom_perm_set.contains(s)));

    // Clean up empty permissions
    if allow.is_empty() {
        if let Some(perms) = settings
            .get_mut("permissions")
            .and_then(|p| p.as_object_mut())
        {
            perms.remove("allow");
        }
    }
    if settings
        .get("permissions")
        .and_then(|p| p.as_object())
        .is_some_and(serde_json::Map::is_empty)
    {
        if let Some(obj) = settings.as_object_mut() {
            obj.remove("permissions");
        }
    }
}

/// Setup repository scaffolding files
///
/// Copies CLAUDE.md, .claude/, .codex/, and .github/ to the workspace.
/// - Fresh install: Copies all files from defaults
/// - Reinstall without force (merge mode): Adds new files, preserves ALL existing files
/// - Reinstall with force (force-merge mode): Updates default files, preserves custom files
/// - Template variables: Substitutes variables in CLAUDE.md
///   - `{{REPO_OWNER}}`, `{{REPO_NAME}}`: Repository info from git remote
///   - `{{LOOM_VERSION}}`, `{{LOOM_COMMIT}}`, `{{INSTALL_DATE}}`: Loom installation metadata
///
/// **CLAUDE.md Handling**:
/// - Full Loom guide is written to `<workspace>/.loom/CLAUDE.md` (with template substitution)
/// - Only a short pointer is injected into root `CLAUDE.md` (between Loom section markers)
/// - If existing root CLAUDE.md has Loom section markers, only the marked section is replaced
/// - If existing root CLAUDE.md has no markers, Loom pointer is appended at the end
/// - All existing root CLAUDE.md content is preserved exactly as-is
/// - Claude Code auto-discovers `.loom/CLAUDE.md` in `.loom/worktrees/issue-N/` via ancestor dirs
///
/// Custom files (files in workspace that don't exist in defaults) are always preserved.
#[allow(clippy::too_many_lines)]
pub fn setup_repository_scaffolding(
    workspace_path: &Path,
    defaults_path: &Path,
    force: bool,
    report: &mut InitReport,
) -> Result<(), String> {
    // Extract repository owner and name for template substitution
    let repo_info = extract_repo_info(workspace_path);
    let (repo_owner, repo_name) = match repo_info {
        Some((owner, name)) => (Some(owner), Some(name)),
        None => (None, None),
    };

    // Get Loom installation metadata from environment variables
    let loom_metadata = LoomMetadata::from_env();

    // Helper to copy directory with force logic and reporting
    // - Fresh install (dst doesn't exist): copy all
    // - Reinstall without force: merge (add new, preserve existing)
    // - Reinstall with force: force-merge (update defaults, preserve custom)
    let copy_directory =
        |src: &Path, dst: &Path, name: &str, report: &mut InitReport| -> Result<(), String> {
            if src.exists() {
                if !dst.exists() {
                    // Fresh install: copy all
                    copy_dir_with_report(src, dst, name, report)
                        .map_err(|e| format!("Failed to copy {name}: {e}"))?;
                } else if force {
                    // Force reinstall: update defaults, preserve custom files
                    force_merge_dir_with_report(src, dst, name, report)
                        .map_err(|e| format!("Failed to force-merge {name}: {e}"))?;
                } else {
                    // Merge reinstall: add new files only, preserve all existing
                    merge_dir_with_report(src, dst, name, report)
                        .map_err(|e| format!("Failed to merge {name}: {e}"))?;
                }
            }
            Ok(())
        };

    // Handle Loom CLAUDE.md content:
    //
    // 1. Write full Loom guide to `<workspace>/.loom/CLAUDE.md` (template substituted)
    //    - Claude Code discovers this automatically when agents work in worktrees
    //    - Always written on install/reinstall (overwrite on reinstall to get latest content)
    //
    // 2. Inject short pointer into root `CLAUDE.md` (between Loom section markers)
    //    - Keeps root CLAUDE.md minimal — saves context budget for non-Loom sessions
    //    - If existing root has markers, only the marked section is replaced
    //    - If existing root has no markers, pointer is appended at the end
    let claude_md_src = defaults_path.join(".loom").join("CLAUDE.md");

    if claude_md_src.exists() {
        // Read the Loom template content
        let loom_content = fs::read_to_string(&claude_md_src)
            .map_err(|e| format!("Failed to read CLAUDE.md template: {e}"))?;

        // Substitute template variables in Loom content
        let loom_substituted = substitute_template_variables(
            &loom_content,
            repo_owner.as_deref(),
            repo_name.as_deref(),
            &loom_metadata,
        );

        // --- Step 1: Write full guide to .loom/CLAUDE.md ---
        let loom_dir = workspace_path.join(".loom");
        // .loom/ should already exist (created earlier in initialize_workspace),
        // but create it if it doesn't to be safe.
        if !loom_dir.exists() {
            fs::create_dir_all(&loom_dir)
                .map_err(|e| format!("Failed to create .loom directory: {e}"))?;
        }
        let loom_claude_md_dst = loom_dir.join("CLAUDE.md");
        let loom_claude_md_existed = loom_claude_md_dst.exists();
        fs::write(&loom_claude_md_dst, &loom_substituted)
            .map_err(|e| format!("Failed to write .loom/CLAUDE.md: {e}"))?;
        if loom_claude_md_existed {
            report.updated.push(".loom/CLAUDE.md".to_string());
        } else {
            report.added.push(".loom/CLAUDE.md".to_string());
        }

        // --- Step 2: Inject short pointer into root CLAUDE.md ---
        let claude_md_dst = workspace_path.join("CLAUDE.md");
        let existed = claude_md_dst.exists();

        // The pointer is a single-line description wrapped in section markers
        let wrapped_pointer = wrap_loom_content(LOOM_ROOT_POINTER);

        let final_content = if existed {
            // Read existing content
            let existing_content = fs::read_to_string(&claude_md_dst)
                .map_err(|e| format!("Failed to read existing CLAUDE.md: {e}"))?;

            // Check if existing file already has Loom section markers
            if existing_content.contains(LOOM_SECTION_START) {
                // Replace just the Loom section with the pointer, preserve everything else
                if let (Some(start_idx), Some(end_idx)) = (
                    existing_content.find(LOOM_SECTION_START),
                    existing_content.find(LOOM_SECTION_END),
                ) {
                    let before = &existing_content[..start_idx];
                    let after_end = end_idx + LOOM_SECTION_END.len();
                    let after = if after_end < existing_content.len() {
                        &existing_content[after_end..]
                    } else {
                        ""
                    };

                    format!("{}{}{}", before.trim_end(), wrapped_pointer, after)
                } else {
                    // Malformed markers - append pointer at end
                    format!("{}\n\n{}", existing_content.trim(), wrapped_pointer)
                }
            } else {
                // No markers exist - append Loom pointer at end
                format!("{}\n\n{}", existing_content.trim(), wrapped_pointer)
            }
        } else {
            // New file - just use wrapped pointer
            wrapped_pointer
        };

        // Only write if we're creating new or content changed
        if existed {
            let current = fs::read_to_string(&claude_md_dst).unwrap_or_default();
            if final_content != current {
                fs::write(&claude_md_dst, &final_content)
                    .map_err(|e| format!("Failed to write CLAUDE.md: {e}"))?;
                if !report.preserved.contains(&"CLAUDE.md".to_string()) {
                    report.updated.push("CLAUDE.md".to_string());
                }
            } else if !report.preserved.contains(&"CLAUDE.md".to_string()) {
                report.preserved.push("CLAUDE.md".to_string());
            }
        } else {
            fs::write(&claude_md_dst, &final_content)
                .map_err(|e| format!("Failed to write CLAUDE.md: {e}"))?;
            report.added.push("CLAUDE.md".to_string());
        }
    }

    // Copy .claude/ directory - always update default commands, preserve custom commands
    // - Fresh install: copy all from defaults
    // - Reinstall: always force-merge (update defaults, preserve custom)
    //
    // This ensures command updates from loom propagate to target repos while
    // preserving any custom commands the project has added.
    // Consistent with .loom/roles/ and .loom/scripts/ behavior.
    //
    // Special handling for settings.json: deep-merge hooks and permissions
    // instead of overwriting, so project-specific hooks are preserved.
    let claude_src = defaults_path.join(".claude");
    let claude_dst = workspace_path.join(".claude");
    if claude_src.exists() {
        // Save existing settings.json before directory copy overwrites it
        let existing_settings = read_existing_settings(&claude_dst.join("settings.json"));

        if claude_dst.exists() {
            // Reinstall: always force-merge to update default commands
            // Custom commands (files not in defaults) are preserved
            force_merge_dir_with_report(&claude_src, &claude_dst, ".claude", report)
                .map_err(|e| format!("Failed to force-merge .claude directory: {e}"))?;
        } else {
            // Fresh install: copy all
            copy_dir_with_report(&claude_src, &claude_dst, ".claude", report)
                .map_err(|e| format!("Failed to copy .claude directory: {e}"))?;
        }

        // If there was an existing settings.json, merge Loom's defaults into it
        // instead of using the overwritten copy
        if let Some(existing) = existing_settings {
            let settings_path = claude_dst.join("settings.json");
            let loom_defaults = read_existing_settings(&settings_path);
            if let Some(loom) = loom_defaults {
                let merged = merge_settings_json(&existing, &loom);
                if let Ok(pretty) = serde_json::to_string_pretty(&merged) {
                    if let Err(e) = fs::write(&settings_path, pretty) {
                        eprintln!("Warning: Failed to write merged settings.json: {e}");
                    }
                }
            }
        }
    }

    // Copy .codex/ directory
    copy_directory(
        &defaults_path.join(".codex"),
        &workspace_path.join(".codex"),
        ".codex",
        report,
    )?;

    // Copy .github/ directory
    copy_directory(
        &defaults_path.join(".github"),
        &workspace_path.join(".github"),
        ".github",
        report,
    )?;

    // Note: The label-external-issues.yml workflow is no longer installed by default.
    // It generated spammy "No jobs were run" emails in single-contributor repos.
    // The workflow is available in defaults/optional/github-workflows/ for manual installation.

    // Note: scripts/ is now copied earlier in initialize_workspace()
    // to .loom/scripts/ along with other .loom-specific files

    // Copy package.json ONLY if workspace doesn't have one
    // (never overwrite existing package.json, even in force mode)
    // This provides stub scripts for pnpm commands referenced in roles
    let package_json_src = defaults_path.join("package.json");
    let package_json_dst = workspace_path.join("package.json");
    if package_json_src.exists() && !package_json_dst.exists() {
        fs::copy(&package_json_src, &package_json_dst)
            .map_err(|e| format!("Failed to copy package.json: {e}"))?;
    }

    // Install loom.sh convenience wrapper at repo root (always update from defaults)
    // This is a thin wrapper around .loom/scripts/daemon.sh that lets
    // users run `./loom.sh` from the repo root instead of the full script path.
    let loom_sh_src = defaults_path.join("loom.sh");
    let loom_sh_dst = workspace_path.join("loom.sh");
    if loom_sh_src.exists() {
        fs::copy(&loom_sh_src, &loom_sh_dst).map_err(|e| format!("Failed to copy loom.sh: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&loom_sh_dst)
                .map_err(|e| format!("Failed to read loom.sh metadata: {e}"))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&loom_sh_dst, perms)
                .map_err(|e| format!("Failed to make loom.sh executable: {e}"))?;
        }
        report.updated.push("loom.sh".to_string());
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_wrap_loom_content() {
        let content = "# Loom Orchestration\n\nLoom content here.";
        let wrapped = wrap_loom_content(content);

        assert!(wrapped.starts_with(LOOM_SECTION_START));
        assert!(wrapped.ends_with(LOOM_SECTION_END));
        assert!(wrapped.contains("Loom content here"));
    }

    #[test]
    fn test_setup_repository_scaffolding_force_mode() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults directory with .claude commands
        fs::create_dir_all(defaults.join(".claude").join("commands")).unwrap();
        fs::write(
            defaults.join(".claude").join("commands").join("loom.md"),
            "loom command from defaults",
        )
        .unwrap();
        fs::write(
            defaults.join(".claude").join("commands").join("builder.md"),
            "builder command from defaults",
        )
        .unwrap();

        // Create existing .claude directory in workspace with custom commands
        fs::create_dir_all(workspace.join(".claude").join("commands")).unwrap();
        fs::write(
            workspace.join(".claude").join("commands").join("custom.md"),
            "my custom command",
        )
        .unwrap();
        fs::write(workspace.join(".claude").join("commands").join("loom.md"), "old loom command")
            .unwrap();

        // Run setup with force=true (force-merge mode)
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        // Verify custom.md was PRESERVED (custom file not in defaults)
        assert!(workspace
            .join(".claude")
            .join("commands")
            .join("custom.md")
            .exists());
        let custom_content =
            fs::read_to_string(workspace.join(".claude").join("commands").join("custom.md"))
                .unwrap();
        assert_eq!(custom_content, "my custom command");

        // Verify loom.md was UPDATED with new content (default file)
        let loom_content =
            fs::read_to_string(workspace.join(".claude").join("commands").join("loom.md")).unwrap();
        assert_eq!(loom_content, "loom command from defaults");

        // Verify builder.md was ADDED (new file from defaults)
        assert!(workspace
            .join(".claude")
            .join("commands")
            .join("builder.md")
            .exists());
    }

    #[test]
    fn test_setup_repository_scaffolding_merge_mode() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults directory with .claude commands
        fs::create_dir_all(defaults.join(".claude").join("commands")).unwrap();
        fs::write(
            defaults.join(".claude").join("commands").join("loom.md"),
            "loom command from defaults",
        )
        .unwrap();
        fs::write(
            defaults.join(".claude").join("commands").join("builder.md"),
            "builder command from defaults",
        )
        .unwrap();

        // Create existing .claude directory in workspace with custom commands
        fs::create_dir_all(workspace.join(".claude").join("commands")).unwrap();
        fs::write(
            workspace.join(".claude").join("commands").join("custom.md"),
            "my custom command",
        )
        .unwrap();
        fs::write(
            workspace.join(".claude").join("commands").join("loom.md"),
            "custom loom command",
        )
        .unwrap();

        // Run setup with force=false (merge mode for .codex/.github, but .claude/ always force-merges)
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify custom.md still exists (preserved)
        assert!(workspace
            .join(".claude")
            .join("commands")
            .join("custom.md")
            .exists());
        let custom_content =
            fs::read_to_string(workspace.join(".claude").join("commands").join("custom.md"))
                .unwrap();
        assert_eq!(custom_content, "my custom command");

        // Verify loom.md was UPDATED with new content (default file)
        // .claude/ always force-merges on reinstall to propagate command updates
        let loom_content =
            fs::read_to_string(workspace.join(".claude").join("commands").join("loom.md")).unwrap();
        assert_eq!(loom_content, "loom command from defaults");

        // Verify builder.md was added (new file)
        assert!(workspace
            .join(".claude")
            .join("commands")
            .join("builder.md")
            .exists());
        let builder_content = fs::read_to_string(
            workspace
                .join(".claude")
                .join("commands")
                .join("builder.md"),
        )
        .unwrap();
        assert_eq!(builder_content, "builder command from defaults");
    }

    #[test]
    fn test_package_json_copied_when_missing() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with package.json
        fs::create_dir_all(&defaults).unwrap();
        fs::write(
            defaults.join("package.json"),
            r#"{"name": "loom-workspace", "scripts": {"test": "echo test"}}"#,
        )
        .unwrap();

        // Workspace has no package.json initially
        assert!(!workspace.join("package.json").exists());

        // Run setup
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify package.json was copied
        assert!(workspace.join("package.json").exists());
        let content = fs::read_to_string(workspace.join("package.json")).unwrap();
        assert!(content.contains("loom-workspace"));
    }

    #[test]
    fn test_package_json_preserved_when_exists() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with package.json
        fs::create_dir_all(&defaults).unwrap();
        fs::write(
            defaults.join("package.json"),
            r#"{"name": "loom-workspace", "scripts": {"test": "echo test"}}"#,
        )
        .unwrap();

        // Create existing package.json in workspace (project-specific)
        fs::write(
            workspace.join("package.json"),
            r#"{"name": "my-rust-project", "scripts": {"build": "cargo build"}}"#,
        )
        .unwrap();

        // Run setup with force=true (should STILL preserve package.json)
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        // Verify package.json was NOT overwritten
        let content = fs::read_to_string(workspace.join("package.json")).unwrap();
        assert!(content.contains("my-rust-project"));
        assert!(!content.contains("loom-workspace"));
    }

    /// Helper to create a standard test setup with a CLAUDE.md template in defaults
    fn setup_test_with_claude_template(
        temp_dir: &TempDir,
        template_content: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let workspace = temp_dir.path().to_path_buf();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with CLAUDE.md template
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(defaults.join(".loom").join("CLAUDE.md"), template_content).unwrap();

        (workspace, defaults)
    }

    #[test]
    fn test_loom_claude_md_written_to_loom_dir() {
        // Verifies full content goes to .loom/CLAUDE.md on fresh install
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_claude_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide\n\nFull guide content here.",
        );

        // Pre-create .loom/ dir (as initialize_workspace normally does)
        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Run setup
        let mut report = InitReport::default();
        setup_repository_scaffolding(&workspace, &defaults, false, &mut report).unwrap();

        // Verify .loom/CLAUDE.md was created with full guide content
        assert!(workspace.join(".loom").join("CLAUDE.md").exists());
        let loom_claude_content =
            fs::read_to_string(workspace.join(".loom").join("CLAUDE.md")).unwrap();
        assert!(loom_claude_content.contains("Loom Orchestration - Repository Guide"));
        assert!(loom_claude_content.contains("Full guide content here"));
        assert!(report.added.contains(&".loom/CLAUDE.md".to_string()));
    }

    #[test]
    fn test_root_claude_md_contains_only_pointer() {
        // Verifies root CLAUDE.md has short pointer, not full guide, on fresh install
        let temp_dir = TempDir::new().unwrap();
        let (workspace, defaults) = setup_test_with_claude_template(
            &temp_dir,
            "# Loom Orchestration - Repository Guide\n\nFull guide content here.",
        );

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // No existing root CLAUDE.md
        assert!(!workspace.join("CLAUDE.md").exists());

        let mut report = InitReport::default();
        setup_repository_scaffolding(&workspace, &defaults, false, &mut report).unwrap();

        // Verify root CLAUDE.md has only the pointer, not the full guide
        assert!(workspace.join("CLAUDE.md").exists());
        let root_content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(root_content.contains(LOOM_SECTION_START));
        assert!(root_content.contains(LOOM_SECTION_END));
        assert!(root_content.contains(LOOM_ROOT_POINTER));
        // Full guide content must NOT be in root CLAUDE.md
        assert!(!root_content.contains("Full guide content here"));
        assert!(report.added.contains(&"CLAUDE.md".to_string()));
    }

    #[test]
    fn test_claude_md_preservation_new_install() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with CLAUDE.md template
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration - Repository Guide\n\nLoom content here.",
        )
        .unwrap();

        // Pre-create .loom/ dir
        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // No existing root CLAUDE.md in workspace
        assert!(!workspace.join("CLAUDE.md").exists());

        // Run setup
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify root CLAUDE.md was created with section markers and short pointer only
        assert!(workspace.join("CLAUDE.md").exists());
        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(content.contains(LOOM_SECTION_START));
        assert!(content.contains(LOOM_SECTION_END));
        assert!(content.contains(LOOM_ROOT_POINTER));
        // Full content must be absent from root
        assert!(!content.contains("Loom content here"));
        assert!(report.added.contains(&"CLAUDE.md".to_string()));

        // Verify .loom/CLAUDE.md was created with full content
        assert!(workspace.join(".loom").join("CLAUDE.md").exists());
        let loom_content = fs::read_to_string(workspace.join(".loom").join("CLAUDE.md")).unwrap();
        assert!(loom_content.contains("Loom content here"));
    }

    #[test]
    fn test_claude_md_preservation_existing_project_content() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with CLAUDE.md template
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration - Repository Guide\n\nNew Loom content.",
        )
        .unwrap();

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Create existing CLAUDE.md with project-specific content (no markers)
        fs::write(
            workspace.join("CLAUDE.md"),
            r"# My Awesome Project

This project does amazing things with Rust.

## Getting Started

Run `cargo run` to start.",
        )
        .unwrap();

        // Run setup - Loom pointer should be appended at end
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify existing content was preserved and Loom pointer appended
        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(content.contains("My Awesome Project"));
        assert!(content.contains("amazing things with Rust"));
        assert!(content.contains(LOOM_SECTION_START));
        assert!(content.contains(LOOM_SECTION_END));
        assert!(content.contains(LOOM_ROOT_POINTER));
        // Full Loom guide must NOT be in root
        assert!(!content.contains("New Loom content"));

        // Project content should come BEFORE Loom section (appended at end)
        let project_pos = content.find("My Awesome Project").unwrap();
        let loom_pos = content.find(LOOM_SECTION_START).unwrap();
        assert!(project_pos < loom_pos);

        // No duplicate content
        assert_eq!(content.matches("My Awesome Project").count(), 1);
    }

    #[test]
    fn test_claude_md_append_when_no_markers() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with CLAUDE.md template
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration - Repository Guide\n\nLoom content here.",
        )
        .unwrap();

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Create existing CLAUDE.md WITHOUT markers (e.g., from previous install or manual creation)
        fs::write(
            workspace.join("CLAUDE.md"),
            r"# Lean Genius Project

Formal mathematics in Lean 4.

## Docker Build Safety

WARNING: Never run `lake build` inside Docker - causes memory corruption.

## Custom Agents

- Erdos: Mathematical proof orchestrator
- Aristotle: Automated theorem prover",
        )
        .unwrap();

        // Run setup
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        // Verify existing content was preserved at top
        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(content.contains("Lean Genius Project"));
        assert!(content.contains("Docker Build Safety"));
        assert!(content.contains("Custom Agents"));

        // Verify Loom pointer was appended at end with markers
        assert!(content.contains(LOOM_SECTION_START));
        assert!(content.contains(LOOM_SECTION_END));
        assert!(content.contains(LOOM_ROOT_POINTER));
        // Full guide must NOT be in root
        assert!(!content.contains("Loom content here"));

        // Verify order: project content comes BEFORE Loom section
        let project_pos = content.find("Lean Genius Project").unwrap();
        let loom_pos = content.find(LOOM_SECTION_START).unwrap();
        assert!(project_pos < loom_pos);

        // Verify no duplicate content or mangling
        assert_eq!(content.matches("Lean Genius Project").count(), 1);
    }

    #[test]
    fn test_claude_md_preservation_update_loom_section_only() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with CLAUDE.md template (simulating upgrade)
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration - Repository Guide\n\nUPDATED Loom content v2.0.",
        )
        .unwrap();

        fs::create_dir_all(workspace.join(".loom")).unwrap();

        // Create existing CLAUDE.md with markers (previous install had full guide in root)
        // This simulates upgrading from old install where full guide was in root CLAUDE.md
        let existing = format!(
            "# My Project\n\nProject docs here.\n\n{LOOM_SECTION_START}\n# Loom Orchestration - Repository Guide\n\nOld Loom content v1.0.\n{LOOM_SECTION_END}"
        );
        fs::write(workspace.join("CLAUDE.md"), existing).unwrap();

        // Run setup with force=true
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        // Verify project content was preserved, Loom section was replaced with short pointer
        let content = fs::read_to_string(workspace.join("CLAUDE.md")).unwrap();
        assert!(content.contains("My Project"));
        assert!(content.contains("Project docs here"));
        // Old full guide content must be gone from root
        assert!(!content.contains("Old Loom content v1.0"));
        // Updated full guide must also NOT be in root
        assert!(!content.contains("UPDATED Loom content v2.0"));
        // Root should now have the short pointer
        assert!(content.contains(LOOM_ROOT_POINTER));

        // Should only have ONE set of markers
        assert_eq!(
            content.matches(LOOM_SECTION_START).count(),
            1,
            "Should have exactly one start marker"
        );
        assert_eq!(
            content.matches(LOOM_SECTION_END).count(),
            1,
            "Should have exactly one end marker"
        );

        // Updated full guide content must be in .loom/CLAUDE.md
        let loom_content = fs::read_to_string(workspace.join(".loom").join("CLAUDE.md")).unwrap();
        assert!(loom_content.contains("UPDATED Loom content v2.0"));
    }

    #[test]
    fn test_loom_claude_md_updated_on_reinstall() {
        // Verifies .loom/CLAUDE.md is overwritten on reinstall with new template content
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        fs::create_dir(workspace.join(".git")).unwrap();
        fs::create_dir_all(defaults.join(".loom")).unwrap();
        fs::write(
            defaults.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration\n\nUpdated content v2.",
        )
        .unwrap();

        // Pre-existing .loom/CLAUDE.md from previous install
        fs::create_dir_all(workspace.join(".loom")).unwrap();
        fs::write(
            workspace.join(".loom").join("CLAUDE.md"),
            "# Loom Orchestration\n\nOld content v1.",
        )
        .unwrap();

        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify .loom/CLAUDE.md was updated with new content
        let loom_content = fs::read_to_string(workspace.join(".loom").join("CLAUDE.md")).unwrap();
        assert!(loom_content.contains("Updated content v2"));
        assert!(!loom_content.contains("Old content v1"));
        assert!(report.updated.contains(&".loom/CLAUDE.md".to_string()));
    }

    #[test]
    fn test_claude_commands_always_updated_on_reinstall() {
        // .claude/ commands should always be force-merged on reinstall (without --force flag)
        // This ensures command updates propagate while custom commands are preserved
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with .claude commands
        fs::create_dir_all(defaults.join(".claude").join("commands")).unwrap();
        fs::write(
            defaults.join(".claude").join("commands").join("loom.md"),
            "loom command v2 with bug fix",
        )
        .unwrap();
        fs::write(
            defaults.join(".claude").join("commands").join("builder.md"),
            "builder command v2",
        )
        .unwrap();

        // Create existing .claude directory in workspace (simulates previous install)
        fs::create_dir_all(workspace.join(".claude").join("commands")).unwrap();
        fs::write(
            workspace.join(".claude").join("commands").join("loom.md"),
            "loom command v1 with bug",
        )
        .unwrap();
        fs::write(
            workspace
                .join(".claude")
                .join("commands")
                .join("my-custom.md"),
            "my project-specific command",
        )
        .unwrap();

        // Run setup WITHOUT force flag (simulates normal reinstall)
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, false, &mut report).unwrap();

        // Verify: loom.md was UPDATED (default command updated with bug fix)
        let loom_content =
            fs::read_to_string(workspace.join(".claude").join("commands").join("loom.md")).unwrap();
        assert_eq!(loom_content, "loom command v2 with bug fix");

        // Verify: builder.md was ADDED (new default command)
        let builder_content = fs::read_to_string(
            workspace
                .join(".claude")
                .join("commands")
                .join("builder.md"),
        )
        .unwrap();
        assert_eq!(builder_content, "builder command v2");

        // Verify: my-custom.md was PRESERVED (custom command not in defaults)
        let custom_content = fs::read_to_string(
            workspace
                .join(".claude")
                .join("commands")
                .join("my-custom.md"),
        )
        .unwrap();
        assert_eq!(custom_content, "my project-specific command");

        // Verify report reflects the changes
        assert!(report
            .updated
            .contains(&".claude/commands/loom.md".to_string()));
        assert!(report
            .added
            .contains(&".claude/commands/builder.md".to_string()));
        assert!(report
            .preserved
            .contains(&".claude/commands/my-custom.md".to_string()));
    }

    // =========================================================================
    // settings.json merge tests
    // =========================================================================

    #[test]
    fn test_merge_settings_fresh_install_no_existing() {
        // When no existing settings.json, Loom defaults are used as-is
        let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": ".loom/hooks/guard-destructive.sh"}]
                }]
            },
            "permissions": {
                "allow": ["Bash(gh:*)", "Bash(git:*)"]
            }
        }"#,
        )
        .unwrap();

        // Empty existing
        let existing: serde_json::Value = serde_json::from_str("{}").unwrap();
        let merged = merge_settings_json(&existing, &loom_defaults);

        // Should have Loom's hooks
        let hooks = merged.get("hooks").unwrap();
        let pre_tool = hooks.get("PreToolUse").unwrap().as_array().unwrap();
        assert_eq!(pre_tool.len(), 1);
        assert_eq!(pre_tool[0]["matcher"], "Bash");

        // Should have Loom's permissions
        let perms = merged
            .get("permissions")
            .unwrap()
            .get("allow")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(perms.len(), 2);
    }

    #[test]
    fn test_merge_settings_preserves_project_hooks() {
        let existing: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Edit",
                    "hooks": [{"type": "command", "command": ".claude/hooks/guard-pdk-files.sh"}]
                }],
                "UserPromptSubmit": [{
                    "matcher": "",
                    "hooks": [{"type": "command", "command": "skill-router.sh"}]
                }]
            },
            "permissions": {
                "allow": ["Bash(gh:*)", "CustomPermission"]
            }
        }"#,
        )
        .unwrap();

        let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": ".loom/hooks/guard-destructive.sh"}]
                }]
            },
            "permissions": {
                "allow": ["Bash(gh:*)", "Bash(git:*)"]
            }
        }"#,
        )
        .unwrap();

        let merged = merge_settings_json(&existing, &loom_defaults);

        // PreToolUse should have both Edit (project) and Bash (Loom) matchers
        let pre_tool = merged["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 2, "Should have both Edit and Bash matchers");

        // Edit matcher should be preserved
        let edit_matcher = pre_tool.iter().find(|m| m["matcher"] == "Edit").unwrap();
        assert_eq!(edit_matcher["hooks"][0]["command"], ".claude/hooks/guard-pdk-files.sh");

        // Bash matcher should be added from Loom
        let bash_matcher = pre_tool.iter().find(|m| m["matcher"] == "Bash").unwrap();
        assert_eq!(bash_matcher["hooks"][0]["command"], ".loom/hooks/guard-destructive.sh");

        // UserPromptSubmit (project-only) should be preserved
        let user_prompt = merged["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(user_prompt.len(), 1);
        assert_eq!(user_prompt[0]["hooks"][0]["command"], "skill-router.sh");

        // Permissions should be unioned (3 unique: gh, git, CustomPermission)
        let perms = merged["permissions"]["allow"].as_array().unwrap();
        assert_eq!(perms.len(), 3, "Should have 3 unique permissions");
        let perm_strs: Vec<&str> = perms.iter().map(|p| p.as_str().unwrap()).collect();
        assert!(perm_strs.contains(&"Bash(gh:*)"));
        assert!(perm_strs.contains(&"Bash(git:*)"));
        assert!(perm_strs.contains(&"CustomPermission"));
    }

    #[test]
    fn test_merge_settings_deduplicates_hooks() {
        // When project already has the Loom hook, don't add it again
        let existing: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": ".loom/hooks/guard-destructive.sh"},
                        {"type": "command", "command": ".claude/hooks/custom-bash-guard.sh"}
                    ]
                }]
            }
        }"#,
        )
        .unwrap();

        let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": ".loom/hooks/guard-destructive.sh"}]
                }]
            }
        }"#,
        )
        .unwrap();

        let merged = merge_settings_json(&existing, &loom_defaults);

        // Should not duplicate the Loom hook
        let bash_hooks = &merged["hooks"]["PreToolUse"][0]["hooks"];
        let hooks_arr = bash_hooks.as_array().unwrap();
        assert_eq!(hooks_arr.len(), 2, "Should not duplicate existing Loom hook");

        // Both hooks should still be present
        let commands: Vec<&str> = hooks_arr
            .iter()
            .map(|h| h["command"].as_str().unwrap())
            .collect();
        assert!(commands.contains(&".loom/hooks/guard-destructive.sh"));
        assert!(commands.contains(&".claude/hooks/custom-bash-guard.sh"));
    }

    #[test]
    fn test_merge_settings_preserves_other_keys() {
        // Keys like enabledPlugins, MCP config, etc. should be preserved
        let existing: serde_json::Value = serde_json::from_str(
            r#"{
            "enabledPlugins": {"some-plugin": true},
            "model": "opus",
            "permissions": {
                "allow": ["CustomPermission"]
            }
        }"#,
        )
        .unwrap();

        let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": ".loom/hooks/guard-destructive.sh"}]
                }]
            },
            "permissions": {
                "allow": ["Bash(gh:*)"]
            }
        }"#,
        )
        .unwrap();

        let merged = merge_settings_json(&existing, &loom_defaults);

        // enabledPlugins and model should be preserved
        assert_eq!(merged["enabledPlugins"]["some-plugin"], true);
        assert_eq!(merged["model"], "opus");

        // Hooks should be added
        assert!(merged.get("hooks").is_some());

        // Permissions should be merged
        let perms = merged["permissions"]["allow"].as_array().unwrap();
        assert_eq!(perms.len(), 2);
    }

    #[test]
    fn test_remove_loom_hooks() {
        let mut settings: serde_json::Value = serde_json::from_str(r#"{
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [
                            {"type": "command", "command": ".loom/hooks/guard-destructive.sh"},
                            {"type": "command", "command": ".claude/hooks/custom-guard.sh"}
                        ]
                    },
                    {
                        "matcher": "Edit",
                        "hooks": [{"type": "command", "command": ".claude/hooks/guard-pdk-files.sh"}]
                    }
                ],
                "UserPromptSubmit": [{
                    "matcher": "",
                    "hooks": [{"type": "command", "command": "skill-router.sh"}]
                }]
            }
        }"#).unwrap();

        remove_loom_hooks(&mut settings);

        // Loom hook should be removed from PreToolUse/Bash
        let bash_hooks = &settings["hooks"]["PreToolUse"][0]["hooks"];
        let hooks_arr = bash_hooks.as_array().unwrap();
        assert_eq!(hooks_arr.len(), 1);
        assert_eq!(hooks_arr[0]["command"], ".claude/hooks/custom-guard.sh");

        // Edit matcher should be untouched
        let edit_hooks = &settings["hooks"]["PreToolUse"][1]["hooks"];
        assert_eq!(edit_hooks.as_array().unwrap().len(), 1);

        // UserPromptSubmit should be untouched
        let user_prompt = &settings["hooks"]["UserPromptSubmit"];
        assert_eq!(user_prompt.as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_remove_loom_hooks_cleans_empty_matchers() {
        // When removing Loom hook leaves a matcher with no hooks, remove the matcher
        let mut settings: serde_json::Value = serde_json::from_str(
            r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": ".loom/hooks/guard-destructive.sh"}]
                }]
            }
        }"#,
        )
        .unwrap();

        remove_loom_hooks(&mut settings);

        // hooks key should be removed entirely since nothing remains
        assert!(
            settings.get("hooks").is_none(),
            "hooks key should be removed when empty, got: {:?}",
            settings
        );
    }

    #[test]
    fn test_remove_loom_permissions() {
        let mut settings: serde_json::Value = serde_json::from_str(
            r#"{
            "permissions": {
                "allow": ["Bash(gh:*)", "Bash(git:*)", "CustomPermission", "WebSearch"]
            }
        }"#,
        )
        .unwrap();

        let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "permissions": {
                "allow": ["Bash(gh:*)", "Bash(git:*)", "WebSearch"]
            }
        }"#,
        )
        .unwrap();

        remove_loom_permissions(&mut settings, &loom_defaults);

        let perms = settings["permissions"]["allow"].as_array().unwrap();
        assert_eq!(perms.len(), 1);
        assert_eq!(perms[0], "CustomPermission");
    }

    #[test]
    fn test_remove_loom_permissions_cleans_empty() {
        let mut settings: serde_json::Value = serde_json::from_str(
            r#"{
            "permissions": {
                "allow": ["Bash(gh:*)"]
            },
            "model": "opus"
        }"#,
        )
        .unwrap();

        let loom_defaults: serde_json::Value = serde_json::from_str(
            r#"{
            "permissions": {
                "allow": ["Bash(gh:*)"]
            }
        }"#,
        )
        .unwrap();

        remove_loom_permissions(&mut settings, &loom_defaults);

        // permissions key should be removed entirely
        assert!(settings.get("permissions").is_none());
        // other keys should be preserved
        assert_eq!(settings["model"], "opus");
    }

    #[test]
    fn test_merge_settings_in_scaffolding_reinstall() {
        // Integration test: verify settings.json is merged during reinstall
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();
        let defaults = temp_dir.path().join("defaults");

        // Setup git repo
        fs::create_dir(workspace.join(".git")).unwrap();

        // Create defaults with .claude/commands and settings.json
        fs::create_dir_all(defaults.join(".claude").join("commands")).unwrap();
        fs::write(defaults.join(".claude").join("commands").join("loom.md"), "loom command")
            .unwrap();
        fs::write(
            defaults.join(".claude").join("settings.json"),
            r#"{
                "hooks": {
                    "PreToolUse": [{
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "command": ".loom/hooks/guard-destructive.sh"}]
                    }]
                },
                "permissions": {
                    "allow": ["Bash(gh:*)", "Bash(git:*)"]
                }
            }"#,
        ).unwrap();

        // Create existing .claude directory with project settings
        fs::create_dir_all(workspace.join(".claude").join("commands")).unwrap();
        fs::write(
            workspace.join(".claude").join("settings.json"),
            r#"{
                "hooks": {
                    "PreToolUse": [{
                        "matcher": "Edit",
                        "hooks": [{"type": "command", "command": ".claude/hooks/guard-pdk.sh"}]
                    }],
                    "UserPromptSubmit": [{
                        "matcher": "",
                        "hooks": [{"type": "command", "command": "skill-router.sh"}]
                    }]
                },
                "permissions": {
                    "allow": ["Bash(gh:*)", "CustomPermission"]
                },
                "enabledPlugins": {"my-plugin": true}
            }"#,
        )
        .unwrap();

        // Run setup (reinstall)
        let mut report = InitReport::default();
        setup_repository_scaffolding(workspace, &defaults, true, &mut report).unwrap();

        // Read the resulting settings.json
        let result_content =
            fs::read_to_string(workspace.join(".claude").join("settings.json")).unwrap();
        let result: serde_json::Value = serde_json::from_str(&result_content).unwrap();

        // PreToolUse should have both matchers (Edit from project, Bash from Loom)
        let pre_tool = result["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 2, "Should have Edit and Bash matchers");

        // Project's Edit matcher should be preserved
        let has_edit = pre_tool.iter().any(|m| m["matcher"] == "Edit");
        assert!(has_edit, "Project's Edit matcher should be preserved");

        // Loom's Bash matcher should be added
        let has_bash = pre_tool.iter().any(|m| m["matcher"] == "Bash");
        assert!(has_bash, "Loom's Bash matcher should be added");

        // Project's UserPromptSubmit should be preserved
        assert!(
            result["hooks"].get("UserPromptSubmit").is_some(),
            "Project's UserPromptSubmit hooks should be preserved"
        );

        // Permissions should be unioned
        let perms = result["permissions"]["allow"].as_array().unwrap();
        let perm_strs: Vec<&str> = perms.iter().map(|p| p.as_str().unwrap()).collect();
        assert!(perm_strs.contains(&"Bash(gh:*)"));
        assert!(perm_strs.contains(&"Bash(git:*)"));
        assert!(perm_strs.contains(&"CustomPermission"));

        // enabledPlugins should be preserved
        assert_eq!(result["enabledPlugins"]["my-plugin"], true);
    }
}
